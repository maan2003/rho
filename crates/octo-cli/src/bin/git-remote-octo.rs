use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};
use reqwest::Url;
use rho_ui_proto::{ClientMessage, GitService, GitTransportRequest, ServerMessage};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().context("missing remote name")?;
    if first == "--bridge" {
        return run_bridge();
    }

    let remote_name = first;
    let url = args.next().context("missing Octo remote URL")?;
    let remote = parse_remote(&url)?;
    let pat_available = remote.github_http_eligible()
        && runtime()?.block_on(query_pat_available(&remote.request.host))?;

    if pat_available {
        run_http_helper(&remote_name, &remote)
    } else {
        run_raw_remote_helper(&remote_name, remote)
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

#[derive(Clone, Debug)]
struct Remote {
    request: GitTransportRequest,
}

impl Remote {
    fn github_http_eligible(&self) -> bool {
        self.request.host == "github.com"
            && self.request.port == 22
            && self.request.user == "git"
            && self.request.repository.split('/').count() == 2
    }

    fn http_url(&self) -> Result<String> {
        anyhow::ensure!(
            self.github_http_eligible(),
            "PAT Git transport is only available for standard GitHub remotes"
        );
        let mut parts = self.request.repository.split('/');
        let owner = parts.next().unwrap();
        let repository = parts.next().unwrap().trim_end_matches(".git");
        Ok(format!("http://localhost/git/{owner}/{repository}.git"))
    }
}

async fn query_pat_available(host: &str) -> Result<bool> {
    let socket = rho_ui_proto::socket_path()?;
    let mut client = rho_ui_proto::client::Client::connect(&socket)
        .await
        .with_context(|| format!("connect to rho daemon at {}", socket.display()))?;
    client
        .send(&ClientMessage::GitTransportQuery {
            host: host.to_owned(),
        })
        .await?;
    match client.recv().await? {
        ServerMessage::GitTransportPolicy { pat_available } => Ok(pat_available),
        message => anyhow::bail!("unexpected Git transport policy reply: {message:?}"),
    }
}

fn run_raw_remote_helper(remote_name: &str, mut remote: Remote) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    run_raw_remote_helper_io(remote_name, &mut remote, stdin.lock(), &mut stdout)
}

fn run_raw_remote_helper_io(
    remote_name: &str,
    remote: &mut Remote,
    input: impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let mut lines = input.lines();
    let mut remote_refs = HashMap::new();
    let mut options = PushOptions::default();
    loop {
        let command = lines
            .next()
            .transpose()?
            .context("Git closed the remote-helper control channel")?;
        match command.as_str() {
            "capabilities" => {
                writeln!(output, "connect")?;
                writeln!(output, "push")?;
                writeln!(output, "option")?;
                writeln!(output)?;
                output.flush()?;
            }
            "connect git-upload-pack" => {
                remote.request.service = GitService::UploadPack;
                drop(lines);
                return runtime()?.block_on(run_transport(remote.request.clone(), true));
            }
            "connect git-receive-pack" => {
                writeln!(output, "fallback")?;
                output.flush()?;
            }
            "list for-push" => {
                remote_refs = local_remote_refs(remote_name)?;
                let mut refs = remote_refs.iter().collect::<Vec<_>>();
                refs.sort_by_key(|(reference, _)| *reference);
                for (reference, object_id) in refs {
                    writeln!(output, "{object_id} {reference}")?;
                }
                writeln!(output)?;
                output.flush()?;
            }
            command if command.starts_with("option ") => {
                let accepted = raw_push_option_supported(command);
                options.record(command, accepted)?;
                writeln!(output, "{}", if accepted { "ok" } else { "unsupported" })?;
                output.flush()?;
            }
            command if command.starts_with("push ") => {
                let batch = collect_batch(command.to_owned(), &mut lines)?;
                let (pushes, has_protocol_options) = parse_push_batch(&batch)?;
                return run_ssh_send_pack(
                    remote,
                    &pushes,
                    &options,
                    has_protocol_options,
                    &remote_refs,
                    output,
                );
            }
            "" => return Ok(()),
            _ => anyhow::bail!("unsupported Git remote-helper command: {command}"),
        }
    }
}

fn raw_push_option_supported(command: &str) -> bool {
    let Some(option) = command.strip_prefix("option ") else {
        return false;
    };
    let (name, value) = option.split_once(' ').unwrap_or((option, ""));
    matches!(
        name,
        "dry-run" | "force" | "atomic" | "progress" | "verbosity" | "cas"
    ) || (name == "pushcert" && value == "false")
}

fn run_bridge() -> Result<()> {
    let planned_refs = std::env::var("RHO_GIT_REFS").context("RHO_GIT_REFS is missing")?;
    let planned_refs = parse_planned_refs(&planned_refs)?;
    let request = GitTransportRequest {
        host: std::env::var("RHO_GIT_HOST").context("RHO_GIT_HOST is missing")?,
        port: std::env::var("RHO_GIT_PORT")
            .context("RHO_GIT_PORT is missing")?
            .parse()
            .context("RHO_GIT_PORT is invalid")?,
        user: std::env::var("RHO_GIT_USER").context("RHO_GIT_USER is missing")?,
        repository: std::env::var("RHO_GIT_REPOSITORY").context("RHO_GIT_REPOSITORY is missing")?,
        service: GitService::ReceivePack,
        planned_refs: Some(planned_refs),
    };
    runtime()?.block_on(run_transport(request, false))
}

fn parse_planned_refs(value: &str) -> Result<Vec<String>> {
    anyhow::ensure!(
        value.len() <= octo_types::MAX_RECEIVE_PACK_COMMAND_BYTES,
        "planned Git ref list is too large"
    );
    let planned_refs = value.lines().map(str::to_owned).collect::<Vec<_>>();
    anyhow::ensure!(!planned_refs.is_empty(), "planned Git ref list is empty");
    anyhow::ensure!(
        planned_refs
            .iter()
            .all(|reference| octo_types::valid_git_ref(reference)),
        "planned Git ref list is invalid"
    );
    anyhow::ensure!(
        planned_refs.iter().collect::<HashSet<_>>().len() == planned_refs.len(),
        "planned Git ref list contains duplicates"
    );
    Ok(planned_refs)
}

async fn run_transport(request: GitTransportRequest, helper_handshake: bool) -> Result<()> {
    let socket = rho_ui_proto::socket_path()?;
    let mut client = rho_ui_proto::client::Client::connect(&socket)
        .await
        .with_context(|| format!("connect to rho daemon at {}", socket.display()))?;
    client
        .send(&ClientMessage::GitTransportRequest {
            request: request.clone(),
        })
        .await?;
    match client.recv().await? {
        ServerMessage::GitTransportReady => {}
        ServerMessage::GitTransportRefused { reason } => anyhow::bail!("{reason}"),
        message => anyhow::bail!("unexpected Git transport reply: {message:?}"),
    }

    if helper_handshake {
        let mut stdout = std::io::stdout();
        writeln!(stdout)?;
        stdout.flush()?;
    }

    let stream = client.into_stream();
    let (mut daemon_read, mut daemon_write) = stream.into_split();
    let service = request.service;
    let upload = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        if service == GitService::ReceivePack {
            copy_validated_receive_pack(&mut stdin, &mut daemon_write).await?;
        } else {
            tokio::io::copy(&mut stdin, &mut daemon_write).await?;
        }
        daemon_write.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    });
    let download = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut daemon_read, &mut stdout).await?;
        stdout.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    });
    upload.await.context("Git transport upload task failed")??;
    download
        .await
        .context("Git transport download task failed")??;
    Ok(())
}

async fn copy_validated_receive_pack<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut prefix = Vec::new();
    loop {
        let mut chunk = [0_u8; 8192];
        let read = reader.read(&mut chunk).await?;
        anyhow::ensure!(read != 0, "truncated git receive-pack request");
        prefix.extend_from_slice(&chunk[..read]);
        match octo_types::parse_receive_pack_commands(&prefix) {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => anyhow::bail!(error),
        }
    }
    writer.write_all(&prefix).await?;
    tokio::io::copy(reader, writer).await?;
    Ok(())
}

struct HttpHelper {
    child: Child,
    input: ChildStdin,
    output: std::io::BufReader<ChildStdout>,
}

impl HttpHelper {
    fn spawn(remote_name: &str, remote: &Remote) -> Result<Self> {
        let socket = octo_types::socket_path()?;
        let socket_type = socket
            .metadata()
            .with_context(|| format!("Octo socket is unavailable at {}", socket.display()))?
            .file_type();
        anyhow::ensure!(
            std::os::unix::fs::FileTypeExt::is_socket(&socket_type),
            "Octo socket path does not refer to a Unix socket"
        );
        let remote_http: PathBuf = option_env!("OCTO_REMOTE_HTTP")
            .map(PathBuf::from)
            .context("git-remote-octo was built without Rho's patched git-remote-http")?;
        anyhow::ensure!(
            Path::new(&remote_http).is_file(),
            "Rho's patched git-remote-http was not found at {}",
            remote_http.display()
        );
        let mut child = Command::new(remote_http)
            .arg(remote_name)
            .arg(remote.http_url()?)
            .env("GIT_HTTP_UNIX_SOCKET", socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start Git's remote-http helper")?;
        let input = child
            .stdin
            .take()
            .context("remote-http stdin unavailable")?;
        let output = std::io::BufReader::new(
            child
                .stdout
                .take()
                .context("remote-http stdout unavailable")?,
        );
        Ok(Self {
            child,
            input,
            output,
        })
    }

    fn send_line(&mut self, line: &str) -> Result<()> {
        writeln!(self.input, "{line}")?;
        self.input.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        anyhow::ensure!(
            self.output.read_line(&mut line)? != 0,
            "git-remote-http closed its protocol stream"
        );
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn relay_until_blank(&mut self, output: &mut impl Write) -> Result<()> {
        loop {
            let line = self.read_line()?;
            writeln!(output, "{line}")?;
            if line.is_empty() {
                output.flush()?;
                return Ok(());
            }
        }
    }

    fn relay_ref_list(
        &mut self,
        output: &mut impl Write,
        refs: &mut HashMap<String, String>,
    ) -> Result<()> {
        loop {
            let line = self.read_line()?;
            writeln!(output, "{line}")?;
            if line.is_empty() {
                output.flush()?;
                return Ok(());
            }
            let mut fields = line.split_ascii_whitespace();
            if let (Some(object_id), Some(reference)) = (fields.next(), fields.next())
                && matches!(object_id.len(), 40 | 64)
                && object_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && reference.starts_with("refs/")
            {
                refs.insert(reference.to_owned(), object_id.to_owned());
            }
        }
    }
}

#[derive(Default)]
struct PushOptions {
    dry_run: bool,
    force: bool,
    atomic: bool,
    progress: Option<bool>,
    verbosity: u8,
    pushcert: Option<String>,
    push_options: Vec<String>,
    service_path: Option<String>,
    force_with_lease: HashMap<String, String>,
}

impl PushOptions {
    fn record(&mut self, command: &str, accepted: bool) -> Result<()> {
        if !accepted {
            return Ok(());
        }
        let Some(rest) = command.strip_prefix("option ") else {
            return Ok(());
        };
        let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
        match name {
            "dry-run" => self.dry_run = value == "true",
            "force" => self.force = value == "true",
            "atomic" => self.atomic = value == "true",
            "progress" => self.progress = Some(value == "true"),
            "verbosity" => self.verbosity = value.parse().unwrap_or(1).min(3),
            "pushcert" => self.pushcert = Some(value.to_owned()),
            "push-option" => self.push_options.push(value.to_owned()),
            "servpath" => self.service_path = Some(value.to_owned()),
            "cas" => {
                let (reference, expected) = value
                    .split_once(':')
                    .context("invalid force-with-lease option")?;
                anyhow::ensure!(
                    octo_types::valid_git_ref(reference)
                        && (expected.is_empty() || octo_types::valid_object_id(expected)),
                    "invalid force-with-lease option"
                );
                anyhow::ensure!(
                    self.force_with_lease
                        .insert(reference.to_owned(), expected.to_owned())
                        .is_none(),
                    "duplicate force-with-lease option"
                );
            }
            _ => {}
        }
        Ok(())
    }
}

fn run_http_helper(remote_name: &str, remote: &Remote) -> Result<()> {
    let mut helper = HttpHelper::spawn(remote_name, remote)?;
    let stdin = std::io::stdin();
    let mut commands = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    let mut options = PushOptions::default();
    let mut remote_refs = HashMap::new();

    let first = commands
        .next()
        .transpose()?
        .context("Git closed the remote-helper control channel")?;
    anyhow::ensure!(
        first == "capabilities",
        "expected remote-helper capabilities"
    );
    helper.send_line(&first)?;
    loop {
        let capability = helper.read_line()?;
        if capability.is_empty() {
            writeln!(stdout)?;
            stdout.flush()?;
            break;
        }
        if !matches!(capability.as_str(), "stateless-connect" | "get") {
            writeln!(stdout, "{capability}")?;
        }
    }

    while let Some(command) = commands.next().transpose()? {
        if command.is_empty() {
            return Ok(());
        }
        if command.starts_with("option ") {
            helper.send_line(&command)?;
            let response = helper.read_line()?;
            options.record(&command, response == "ok")?;
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
            continue;
        }
        if matches!(command.as_str(), "list" | "list for-push") {
            helper.send_line(&command)?;
            if command == "list for-push" {
                remote_refs.clear();
                helper.relay_ref_list(&mut stdout, &mut remote_refs)?;
            } else {
                helper.relay_until_blank(&mut stdout)?;
            }
            continue;
        }
        if command.starts_with("fetch ") {
            let batch = collect_batch(command, &mut commands)?;
            send_batch(&mut helper, &batch)?;
            helper.relay_until_blank(&mut stdout)?;
            continue;
        }
        if command.starts_with("push ") {
            let batch = collect_batch(command, &mut commands)?;
            let (pushes, has_protocol_options) = parse_push_batch(&batch)?;
            if pushes_use_http(&pushes) {
                send_batch(&mut helper, &batch)?;
                helper.relay_until_blank(&mut stdout)?;
                continue;
            }
            let _ = helper.child.kill();
            let _ = helper.child.wait();
            return run_ssh_send_pack(
                remote,
                &pushes,
                &options,
                has_protocol_options,
                &remote_refs,
                &mut stdout,
            );
        }
        anyhow::bail!("unsupported git-remote-http command: {command}");
    }
    Ok(())
}

fn collect_batch(
    first: String,
    commands: &mut impl Iterator<Item = std::io::Result<String>>,
) -> Result<Vec<String>> {
    let mut batch = vec![first];
    loop {
        let line = commands
            .next()
            .transpose()?
            .context("truncated remote-helper command batch")?;
        if line.is_empty() {
            return Ok(batch);
        }
        batch.push(line);
    }
}

fn send_batch(helper: &mut HttpHelper, batch: &[String]) -> Result<()> {
    for command in batch {
        helper.send_line(command)?;
    }
    helper.send_line("")
}

struct PushCommand {
    refspec: String,
    destination: String,
}

fn local_remote_refs(remote_name: &str) -> Result<HashMap<String, String>> {
    const MAX_LOCAL_REMOTE_REFS: usize = 4096;
    let prefix = format!("refs/remotes/{remote_name}/");
    if !octo_types::valid_git_ref(&format!("{prefix}probe")) {
        return Ok(HashMap::new());
    }
    let output = Command::new("git")
        .args([
            "for-each-ref",
            &format!("--count={}", MAX_LOCAL_REMOTE_REFS + 1),
            "--format=%(objectname) %(refname)",
            &prefix,
        ])
        .output()
        .context("read local remote-tracking refs")?;
    anyhow::ensure!(
        output.status.success(),
        "read local remote-tracking refs: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let output =
        std::str::from_utf8(&output.stdout).context("local remote-tracking refs are not UTF-8")?;
    anyhow::ensure!(
        output.lines().count() <= MAX_LOCAL_REMOTE_REFS,
        "too many local remote-tracking refs"
    );
    let mut refs = HashMap::new();
    for line in output.lines() {
        let (object_id, reference) = line
            .split_once(' ')
            .context("invalid local remote-tracking ref")?;
        let suffix = reference
            .strip_prefix(&prefix)
            .context("local remote-tracking ref escaped its namespace")?;
        if suffix == "HEAD" {
            continue;
        }
        let destination = format!("refs/heads/{suffix}");
        anyhow::ensure!(
            octo_types::valid_object_id(object_id) && octo_types::valid_git_ref(&destination),
            "invalid local remote-tracking ref"
        );
        refs.insert(destination, object_id.to_owned());
    }
    Ok(refs)
}

fn pushes_use_http(pushes: &[PushCommand]) -> bool {
    !pushes.is_empty()
        && pushes
            .iter()
            .all(|push| push.destination.starts_with("refs/heads/rho/"))
}

fn force_leases(
    pushes: &[PushCommand],
    options: &PushOptions,
    remote_refs: &HashMap<String, String>,
) -> Result<Vec<String>> {
    anyhow::ensure!(
        options
            .force_with_lease
            .keys()
            .all(|reference| pushes.iter().any(|push| &push.destination == reference)),
        "force-with-lease names a ref outside the push batch"
    );
    Ok(pushes
        .iter()
        .filter_map(|push| {
            if let Some(expected) = options.force_with_lease.get(&push.destination) {
                return Some(format!(
                    "--force-with-lease={}:{}",
                    push.destination, expected
                ));
            }
            let refspec = push.refspec.strip_prefix('+').unwrap_or(&push.refspec);
            let source = refspec
                .split_once(':')
                .map(|(source, _)| source)
                .unwrap_or("");
            (options.force || push.refspec.starts_with('+') || source.is_empty()).then(|| {
                let expected = remote_refs
                    .get(&push.destination)
                    .map(String::as_str)
                    .unwrap_or("");
                format!("--force-with-lease={}:{}", push.destination, expected)
            })
        })
        .collect())
}

fn send_pack_refspecs(pushes: &[PushCommand], force_all: bool) -> Vec<&str> {
    pushes
        .iter()
        .map(|push| {
            if force_all || push.refspec.starts_with('+') {
                push.refspec.strip_prefix('+').unwrap_or(&push.refspec)
            } else {
                push.refspec.as_str()
            }
        })
        .collect()
}

fn parse_push_batch(batch: &[String]) -> Result<(Vec<PushCommand>, bool)> {
    let mut pushes = Vec::new();
    let mut destinations = HashSet::new();
    let mut has_protocol_options = false;
    for command in batch {
        let Some(refspec) = command.strip_prefix("push ") else {
            has_protocol_options = true;
            continue;
        };
        let refspec_without_force = refspec.strip_prefix('+').unwrap_or(refspec);
        let (_, destination) = refspec_without_force
            .split_once(':')
            .context("invalid remote-helper push refspec")?;
        anyhow::ensure!(
            octo_types::valid_git_ref(destination),
            "invalid push destination ref"
        );
        anyhow::ensure!(
            destinations.insert(destination.to_owned()),
            "duplicate push destination ref"
        );
        pushes.push(PushCommand {
            refspec: refspec.to_owned(),
            destination: destination.to_owned(),
        });
    }
    anyhow::ensure!(!pushes.is_empty(), "empty remote-helper push batch");
    let plan_len = pushes.iter().try_fold(0_usize, |length, push| {
        length.checked_add(push.destination.len() + 1)
    });
    anyhow::ensure!(
        plan_len.is_some_and(|length| length <= octo_types::MAX_RECEIVE_PACK_COMMAND_BYTES),
        "planned Git ref list is too large"
    );
    Ok((pushes, has_protocol_options))
}

fn run_ssh_send_pack(
    remote: &Remote,
    pushes: &[PushCommand],
    options: &PushOptions,
    has_protocol_options: bool,
    remote_refs: &HashMap<String, String>,
    output: &mut impl Write,
) -> Result<()> {
    if options
        .pushcert
        .as_deref()
        .is_some_and(|value| value != "false")
        || !options.push_options.is_empty()
        || options.service_path.is_some()
        || has_protocol_options
    {
        for push in pushes {
            writeln!(
                output,
                "error {} client SSH transport does not support signed pushes or push options",
                push.destination
            )?;
        }
        writeln!(output)?;
        output.flush()?;
        return Ok(());
    }

    let executable = std::env::current_exe().context("resolve git-remote-octo executable")?;
    let receive_pack = format!("{} --bridge", shell_quote(&executable.to_string_lossy()));
    let mut command = Command::new("git");
    command
        .arg("send-pack")
        .arg("--helper-status")
        .arg(format!("--receive-pack={receive_pack}"));
    if options.dry_run {
        command.arg("--dry-run");
    }
    if options.atomic {
        command.arg("--atomic");
    }
    if let Some(progress) = options.progress {
        command.arg(if progress {
            "--progress"
        } else {
            "--no-progress"
        });
    }
    for _ in 1..options.verbosity {
        command.arg("--verbose");
    }
    command.args(force_leases(pushes, options, remote_refs)?);
    command
        .arg(".")
        .args(send_pack_refspecs(pushes, options.force))
        .env("RHO_GIT_HOST", &remote.request.host)
        .env("RHO_GIT_PORT", remote.request.port.to_string())
        .env("RHO_GIT_USER", &remote.request.user)
        .env("RHO_GIT_REPOSITORY", &remote.request.repository)
        .env(
            "RHO_GIT_REFS",
            pushes
                .iter()
                .map(|push| push.destination.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let result = command
        .output()
        .context("run git send-pack over client SSH")?;
    output.write_all(&result.stdout)?;
    if !result.status.success() && result.stdout.is_empty() {
        for push in pushes {
            writeln!(
                output,
                "error {} SSH send-pack exited with {}",
                push.destination, result.status
            )?;
        }
    }
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_remote(value: &str) -> Result<Remote> {
    anyhow::ensure!(
        !value
            .split('/')
            .any(|component| matches!(component, "." | "..")),
        "invalid SSH Git repository path"
    );
    let url =
        Url::parse(value).context("Octo remote must be octo://[USER@]HOST[:PORT]/REPOSITORY")?;
    if url.scheme() != "octo"
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("Octo remote must be octo://[USER@]HOST[:PORT]/REPOSITORY");
    }
    let host = url
        .host_str()
        .filter(|host| valid_host(host))
        .context("invalid or missing SSH Git host")?;
    let user = if url.username().is_empty() {
        "git"
    } else {
        url.username()
    };
    anyhow::ensure!(valid_user(user), "invalid SSH Git user");
    let repository = url.path().trim_start_matches('/');
    anyhow::ensure!(
        valid_repository(repository),
        "invalid SSH Git repository path"
    );

    Ok(Remote {
        request: GitTransportRequest {
            host: host.to_owned(),
            port: url.port().unwrap_or(22),
            user: user.to_owned(),
            repository: repository.to_owned(),
            service: GitService::UploadPack,
            planned_refs: None,
        },
    })
}

fn valid_host(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn valid_user(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_repository(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.starts_with(['-', '/'])
        && !value.contains("//")
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.starts_with('-')
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_compatibility_remote() {
        let remote = parse_remote("octo://github.com/acme/library.git").unwrap();
        assert!(remote.github_http_eligible());
        assert_eq!(
            remote.http_url().unwrap(),
            "http://localhost/git/acme/library.git"
        );
    }

    #[test]
    fn parses_generic_ssh_remote() {
        let remote = parse_remote("octo://deploy@git.example.test:2222/team/project.git").unwrap();
        assert!(!remote.github_http_eligible());
        assert_eq!(remote.request.host, "git.example.test");
        assert_eq!(remote.request.port, 2222);
        assert_eq!(remote.request.user, "deploy");
        assert_eq!(remote.request.repository, "team/project.git");
    }

    #[test]
    fn rejects_unsafe_remote_fields() {
        for value in [
            "octo://gitlab.example",
            "octo://user:password@git.example/repo",
            "octo://git.example/../repo",
            "octo://git.example/-oProxyCommand=bad",
            "https://github.com/acme/repo",
        ] {
            assert!(parse_remote(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn routes_push_batches_by_destination_namespace() {
        let (rho, options) =
            parse_push_batch(&["push HEAD:refs/heads/rho/test".to_owned()]).unwrap();
        assert!(!options);
        assert!(pushes_use_http(&rho));

        let (mixed, options) = parse_push_batch(&[
            "push HEAD:refs/heads/rho/test".to_owned(),
            "push HEAD:refs/heads/main".to_owned(),
            "option unknown value".to_owned(),
        ])
        .unwrap();
        assert!(options);
        assert!(!pushes_use_http(&mixed));

        for invalid in [
            vec![
                "push HEAD:refs/heads/main".to_owned(),
                "push HEAD:refs/heads/main".to_owned(),
            ],
            vec!["push HEAD:refs/heads/../main".to_owned()],
        ] {
            assert!(parse_push_batch(&invalid).is_err());
        }
    }

    #[test]
    fn accepts_only_send_pack_options_the_routed_path_preserves() {
        for command in [
            "option dry-run true",
            "option force true",
            "option atomic true",
            "option progress false",
            "option verbosity 2",
            "option pushcert false",
            "option cas refs/heads/main:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "option cas refs/heads/new:",
        ] {
            assert!(raw_push_option_supported(command), "rejected {command}");
        }
        for command in [
            "option pushcert true",
            "option push-option ci.skip",
            "option servpath git-receive-pack",
        ] {
            assert!(!raw_push_option_supported(command), "accepted {command}");
        }
    }

    #[test]
    fn no_pat_receive_pack_falls_back_to_plannable_push_protocol() {
        let mut remote = parse_remote("octo://github.com/acme/library.git").unwrap();
        let input = b"capabilities\nconnect git-receive-pack\nlist for-push\n\n";
        let mut output = Vec::new();
        run_raw_remote_helper_io(
            "octo://github.com/acme/library.git",
            &mut remote,
            &input[..],
            &mut output,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "connect\npush\noption\n\nfallback\n\n"
        );
    }

    #[test]
    fn validates_destination_plan_crossing_the_send_pack_bridge() {
        assert_eq!(
            parse_planned_refs("refs/heads/main\nrefs/tags/v1").unwrap(),
            ["refs/heads/main", "refs/tags/v1"]
        );
        for invalid in ["", "refs/heads/main\nrefs/heads/main", "refs/heads/../main"] {
            assert!(parse_planned_refs(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn shell_quotes_receive_pack_program() {
        assert_eq!(shell_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn forced_updates_and_deletions_preserve_exact_expectations() {
        let (pushes, _) = parse_push_batch(&[
            "push +HEAD:refs/heads/main".to_owned(),
            "push :refs/heads/old".to_owned(),
            "push HEAD:refs/heads/new".to_owned(),
        ])
        .unwrap();
        let old = "a".repeat(40);
        let refs = HashMap::from([
            ("refs/heads/main".to_owned(), old.clone()),
            ("refs/heads/new".to_owned(), old.clone()),
        ]);
        assert_eq!(
            force_leases(&pushes, &PushOptions::default(), &refs).unwrap(),
            vec![
                format!("--force-with-lease=refs/heads/main:{old}"),
                "--force-with-lease=refs/heads/old:".to_owned(),
            ]
        );
        assert_eq!(
            send_pack_refspecs(&pushes, false),
            vec![
                "HEAD:refs/heads/main",
                ":refs/heads/old",
                "HEAD:refs/heads/new"
            ]
        );
    }

    #[test]
    fn explicit_force_with_lease_options_survive_transport_selection() {
        let (pushes, _) = parse_push_batch(&[
            "push HEAD:refs/heads/main".to_owned(),
            "push HEAD:refs/heads/new".to_owned(),
        ])
        .unwrap();
        let expected = "a".repeat(40);
        let mut options = PushOptions::default();
        options
            .record(&format!("option cas refs/heads/main:{expected}"), true)
            .unwrap();
        options.record("option cas refs/heads/new:", true).unwrap();
        assert_eq!(
            force_leases(&pushes, &options, &HashMap::new()).unwrap(),
            [
                format!("--force-with-lease=refs/heads/main:{expected}"),
                "--force-with-lease=refs/heads/new:".to_owned(),
            ]
        );
    }
}
