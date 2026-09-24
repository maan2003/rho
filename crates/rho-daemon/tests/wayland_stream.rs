//! A real daemon, workset worker, Sway capture, and MoQ over the GUI
//! connection.
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rho_agent_host_proto::{Opened, host, read_frame, write_open};
use rho_agents_client::protocol as agents;
use rho_agents_client::protocol::NewAgent;

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn desktop_status(
    control: &mut tokio::io::BufReader<tokio::net::UnixStream>,
) -> Result<(bool, u64, u64)> {
    let rho_desktop_proto::Response::Status {
        streaming,
        composed,
        encoded,
    } = rho_desktop_proto::local::request(control, rho_desktop_proto::Request::Status).await?
    else {
        anyhow::bail!("unexpected desktop status")
    };
    Ok((streaming, composed, encoded))
}
fn main() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let runtime = temp.path().join("runtime");
    std::fs::create_dir(&runtime)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let bin = root.join("target/debug");
    let rho = bin.join("rho");
    ensure!(rho.is_file(), "build rho and rho-agent-worker first");
    let socket = temp.path().join("rho.sock");
    let log = temp.path().join("daemon.log");
    let out = std::fs::File::create(&log)?;
    let mut command = Command::new(&rho);
    command
        .args(["daemon", "--iroh", "--socket-path"])
        .arg(&socket)
        .arg("--claude-config-dir")
        .arg(temp.path().join("claude"))
        .arg("--extra-before-path")
        .arg(&bin)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(out);
    let mut daemon = Child(command.spawn()?);
    let result=tokio::runtime::Runtime::new()?.block_on(async {
        let deadline=tokio::time::Instant::now()+Duration::from_secs(60);
        let (mut local,endpoint)=loop {
            if let Some(status)=daemon.0.try_wait()? { anyhow::bail!("daemon exited: {status}"); }
            let text=std::fs::read_to_string(&log)?;
            let endpoint=text.lines().find_map(|line|line.strip_prefix("rho daemon iroh endpoint: ")).map(str::to_owned);
            if let Some(endpoint)=endpoint {
                if let Ok(stream)=rho_rpc::connect_unix(&socket).await { break (stream,endpoint); }
            }
            ensure!(tokio::time::Instant::now()<deadline,"daemon startup timed out");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        write_open(&mut local,&host::Open::Desktops).await?;
        let repo=temp.path().join("repo");
        std::fs::create_dir(&repo)?;
        ensure!(Command::new("git").args(["init","-q","-b","main"]).arg(&repo).status()?.success(),"git init failed");
        ensure!(Command::new("git").args(["-c","user.name=Test","-c","user.email=test@localhost","commit","-q","--allow-empty","-m","init"]).current_dir(&repo).status()?.success(),"git commit failed");
        let agent=rho_agent_host_proto::client::call(&socket,NewAgent {
            role:Default::default(), start:rho_agents_client::protocol::StartMode::NewOn { repo:camino::Utf8PathBuf::from_path_buf(repo).unwrap(),revset:"@".into() },
            mode:rho_agent_types::WorksetMode::Exposed,content:None,
        }).await.context("agent creation failed")?;
        let desktop_name = "preview".to_owned();
        let desktop_directory = runtime.join("rho-desktop/agents").join(agent.encoded());
        let desktop_program=std::env::var_os("RHO_AGENT_DESKTOP_BIN").map(PathBuf::from)
            .unwrap_or_else(||PathBuf::from("rho-agent-desktop"));
        let config=temp.path().join("desktop.kdl");
        std::fs::write(&config,r##"animations { off; }
hotkey-overlay { skip-at-startup; }
xwayland-satellite { off; }
layout { background-color "#315b97"; }
"##)?;

        let desktop_log=temp.path().join("desktop.log");
        let mut desktop=Child(Command::new(&desktop_program)
            .env("XDG_RUNTIME_DIR",&runtime).env("LIBGL_ALWAYS_SOFTWARE","1").env("RHO_AGENT_ID",agent.encoded())
            .args(["--headless","--name",&desktop_name,"--width","640","--height","480","--scale","1","--config"])
            .arg(&config).stdout(Stdio::null()).stderr(std::fs::File::create(&desktop_log)?).spawn()?);
        let deadline=tokio::time::Instant::now()+Duration::from_secs(20);
        while !desktop_directory.join(format!("{desktop_name}.json")).exists() {
            ensure!(desktop.0.try_wait()?.is_none(),"desktop exited: {}",std::fs::read_to_string(&desktop_log)?);
            ensure!(tokio::time::Instant::now()<deadline,"desktop startup timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let descriptor:serde_json::Value=serde_json::from_slice(&std::fs::read(desktop_directory.join(format!("{desktop_name}.json")))?)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                { let sessions = read_frame::<_, Vec<rho_agent_host_proto::DesktopSession>>(&mut local).await?;
                    ensure!(sessions == vec![rho_agent_host_proto::DesktopSession { agent: agent.encoded(), name: desktop_name.clone() }], "incorrect desktop advertisement: {sessions:?}");
                    break;
                }
            }
            Ok::<_,anyhow::Error>(())
        }).await??;
        let mut status=tokio::io::BufReader::new(rho_desktop_proto::local::connect(descriptor["socket"].as_str().unwrap()).await?);
        rho_desktop_proto::local::request(&mut status,rho_desktop_proto::Request::Hello{version:rho_desktop_proto::VERSION}).await?;
        ensure!(desktop_status(&mut status).await?==(false,0,0),"video work exists without subscriber");
        let endpoint:iroh::EndpointId=endpoint.parse()?;
        let client=rho_rpc::bind_ephemeral_iroh_client().await?;
        rho_agent_host_proto::client::call(&socket,host::IrohTrustInMemory {endpoint_id:client.id().to_string()}).await.context("trust")?;
        let connection=tokio::time::timeout(Duration::from_secs(40),client.connect(endpoint,rho_agent_host_proto::IROH_ALPN)).await??;
        ensure!(rho_rpc::authenticate_iroh_client(&connection,client.id()).await?==rho_iroh_auth::ClientAuthResult::Approved,"auth failed");
        let mux=rho_rpc::media::Mux::new(connection.clone());
        let m=mux.clone(); let uni=tokio::spawn(async move {m.receive_uni().await});
        let m=mux.clone(); let bi=tokio::spawn(async move {m.receive_bi().await});
        let transport=mux.session(1)?;
        let (send,recv)=connection.open_bi().await?;
        let mut input=rho_rpc::Stream::new(recv,send);
        write_open(&mut input,&host::Open::Wayland {media_id:1,agent:agent.encoded(),session:desktop_name.clone()}).await?;
        ensure!(matches!(read_frame::<_,Opened>(&mut input).await?,Opened::Ready),"open failed");
        let origin=rho_desktop_media::media::origin();
        let media=rho_desktop_media::media::subscribe(transport,origin.clone()).await.context("fixed video stream")?;
        let mut announced=origin.consume().announced();
        announced.next().await.context("no app announcement")?;
        let broadcast=origin.consume().request_broadcast("app").await?;
        let track=broadcast.track("video")?;
        let mut video=track.subscribe(None).await.context("subscribe video")?.ordered();
        let result=tokio::time::timeout(Duration::from_secs(10),async {
            let mut group=video.next_group().await?.context("no video group")?;
            let packet=group.read_frame().await?.context("no frame")?;
            let image=rho_desktop_media::codec::Decoder::new()?.decode(&packet.payload)?.context("no decoded image")?;
            ensure!((image.width,image.height)==(640,480),"wrong image size");
            for (got,want) in image.bgra[..4].iter().zip([0x97u8,0x5b,0x31,255]) {
                ensure!((*got as i16-want as i16).abs()<12,"wrong decoded pixel");
            }
            Ok::<(),anyhow::Error>(())
        }).await?;
        if let Err(error)=result {
            eprintln!("media error: {error:#}");
            if let Ok(Ok((frame,_)))=tokio::time::timeout(Duration::from_millis(100),rho_rpc::read_frame::<_,rho_desktop_proto::Packet>(&mut input,65536)).await {
                let rho_desktop_proto::Packet::Error(error)=frame; eprintln!("desktop: {error}");
            }
            return Err(error);
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let settled=desktop_status(&mut status).await?;
        ensure!(settled.0 && settled.2>=1,"no active encoder");
        tokio::time::sleep(Duration::from_millis(300)).await;
        ensure!(desktop_status(&mut status).await?==settled,"static desktop continued composing or encoding");
        // A late viewer must get a fresh independently decodable group even when
        // the desktop is static and the old keyframe has aged out.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let transport2=mux.session(2)?;
        let (send2,recv2)=connection.open_bi().await?;
        let mut input2=rho_rpc::Stream::new(recv2,send2);
        write_open(&mut input2,&host::Open::Wayland {media_id:2,agent:agent.encoded(),session:desktop_name.clone()}).await?;
        ensure!(matches!(read_frame::<_,Opened>(&mut input2).await?,Opened::Ready),"second viewer open failed");
        let origin2=rho_desktop_media::media::origin();
        let media2=rho_desktop_media::media::subscribe(transport2,origin2.clone()).await?;
        let mut announcements2=origin2.consume().announced();
        announcements2.next().await.context("missing late-join broadcast")?;
        let broadcast2=origin2.consume().request_broadcast("app").await?;
        let mut video2=broadcast2.track("video")?.subscribe(None).await?.ordered();
        tokio::time::timeout(Duration::from_secs(5),async {
            let mut group=video2.next_group().await?.context("late viewer has no keyframe group")?;
            let packet=group.read_frame().await?.context("late viewer has no frame")?;
            let image=rho_desktop_media::codec::Decoder::new()?.decode(&packet.payload)?.context("late viewer decode")?;
            ensure!((image.width,image.height)==(640,480),"late viewer wrong dimensions");
            Ok::<_,anyhow::Error>(())
        }).await??;
        drop(video2);drop(input2);media2.abort(moq_net::Error::Cancel);
        tokio::time::sleep(Duration::from_millis(100)).await;
        ensure!(desktop_status(&mut status).await?.0,"one viewer detach stopped another viewer");
        drop(video);
        drop(input);
        media.abort(moq_net::Error::Cancel);
        let deadline=tokio::time::Instant::now()+Duration::from_secs(4);
        loop {
            if !desktop_status(&mut status).await?.0 {break;}
            ensure!(tokio::time::Instant::now()<deadline,"capture survived final unsubscribe");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let stopped=desktop_status(&mut status).await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        ensure!(desktop_status(&mut status).await?==stopped,"video work survived unsubscribe");
        let (send,recv)=connection.open_bi().await?;
        let mut rpc=rho_rpc::Stream::new(recv,send);
        rho_agent_host_proto::call(&mut rpc,agents::QuotaUsage).await.context("RPC failed after detach")?;
        uni.abort(); bi.abort();
        client.close().await;
        // A second named desktop is advertised, then excluded after a crash
        // even though SIGKILL leaves its manifest behind.
        let mut second = Child(Command::new(&desktop_program)
            .env("XDG_RUNTIME_DIR", &runtime).env("RHO_AGENT_ID", agent.encoded())
            .args(["--headless", "--name", "browser", "--width", "128", "--height", "96", "--scale", "1", "--config"])
            .arg(&config).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                { let sessions = read_frame::<_, Vec<rho_agent_host_proto::DesktopSession>>(&mut local).await?;
                    if sessions.len() == 2 {
                        ensure!(sessions.iter().all(|session| session.agent == agent.encoded()), "wrong owner");
                        ensure!(sessions.iter().map(|session| session.name.as_str()).collect::<Vec<_>>() == vec!["browser", "preview"], "wrong session names");
                        break;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        }).await??;
        second.0.kill()?;
        second.0.wait()?;
        ensure!(desktop_directory.join("browser.json").exists(), "crash fixture did not leave an advertisement");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                { let sessions = read_frame::<_, Vec<rho_agent_host_proto::DesktopSession>>(&mut local).await?;
                    if sessions == vec![rho_agent_host_proto::DesktopSession { agent: agent.encoded(), name: desktop_name.clone() }] { break; }
                }
            }
            Ok::<_, anyhow::Error>(())
        }).await??;
        if let Some(path) = std::env::var_os("RHO_WAYLAND_TEST_PREVIEW") {
            std::fs::write(path, serde_json::to_vec(&serde_json::json!({
                "endpoint":endpoint.to_string(),"agent":agent.encoded(),"socket":socket,"runtime":runtime
            }))?)?;
            tokio::signal::ctrl_c().await?;
        }
        println!("wayland_stream passed: session advertisements and crash cleanup; in-process desktop VP9 frame through direct daemon relay; idle/static/unsubscribe counters verified; RPC survived viewer detach");
        Ok::<(),anyhow::Error>(())
    });
    if result.is_err() {
        eprintln!("{}", std::fs::read_to_string(log).unwrap_or_default());
    }
    result
}
