//! Making an agent: what the draft means, and what it takes to start one.
//!
//! A draft is four answers — a working directory, a base, a runtime and a
//! first message — and every way one of them can be refused is here, in
//! the words the reader is shown. What the shell keeps is the screen and
//! the buffers; what it asks of this module is whether the answers make a
//! workset or an agent in one, and on which host.

use camino::Utf8PathBuf;
use rho_agent_hosts::{HostId, Hosts};
use rho_agent_types::{ContentPart, WorksetId, WorksetKind};

use crate::protocol::rho2::{CreateWorkset, Eval};
use crate::snippets::{self, Runtime};

/// The user-facing name for selecting the first available conventional base.
pub const DEFAULT_START: &str = "auto";
/// The git revision represented by [`DEFAULT_START`]: none, so the agent
/// starts where a fresh clone is born, on the remote's default branch.
pub const AUTO_BASE_REV: &str = "";
/// How the start field's target is interpreted; cycled with Shift-Tab while
/// the cursor is in the field. The field label shows the current mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFieldMode {
    /// A fresh clone with the target revision checked out.
    NewOn,
    /// The same workset as the target agent, in its directory.
    Join,
    /// The workdir itself, in place: no clone, and no target.
    Here,
}

/// What the agents map says about the label in the start field: which host
/// that agent is on, and its workset and directory. Both are the map's to
/// answer, so the caller looks them up and hands them in.
#[derive(Clone, Debug, Default)]
pub struct StartBase {
    pub host: Option<HostId>,
    pub agent: Option<(WorksetId, Utf8PathBuf)>,
}

/// Where a new agent starts. Each carries exactly what it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// A fresh isolated workset holding a clone of `repo` (a URL or a
    /// host-side path) at `rev`.
    NewOn { repo: Utf8PathBuf, rev: String },
    /// Beside another agent, in its workset and directory, seeing its
    /// edits at once.
    Join {
        workset: WorksetId,
        cwd: Utf8PathBuf,
    },
    /// A host workset: the host as it is, in `cwd`.
    Here { cwd: Utf8PathBuf },
}

/// What a start asks of the host.
#[derive(Clone, Debug, PartialEq)]
pub enum Creation {
    Workset(CreateWorkset),
    Eval(Eval),
}

impl Start {
    /// The call that starts it, with the user's first message, which the
    /// host mails to the agent the code creates.
    pub fn creation(&self, runtime: &Runtime, message: Option<Vec<ContentPart>>) -> Creation {
        match self {
            Self::NewOn { repo, rev } => Creation::Workset(CreateWorkset {
                kind: WorksetKind::Isolated,
                code: snippets::new_agent_on(repo, rev, runtime),
                message,
            }),
            Self::Here { cwd } => Creation::Workset(CreateWorkset {
                kind: WorksetKind::Host { cwd: cwd.clone() },
                code: snippets::new_agent_in(cwd, runtime),
                message,
            }),
            Self::Join { workset, cwd } => Creation::Eval(Eval {
                workset: *workset,
                code: snippets::new_agent_in(cwd, runtime),
                message,
            }),
        }
    }
}

/// Whether text names a repository to clone rather than a place on a
/// machine: a URL scheme, or scp-style `git@host:path`.
pub fn is_repository_url(argument: &str) -> bool {
    argument.contains("://") || argument.starts_with("git@")
}

/// What the Workdir field names: a repository to clone, which any host
/// can, or a directory on one machine. Projects are the desk's, not a
/// host's, so a project name resolves to its URL and says nothing about
/// where the agent runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workdir {
    pub path: Utf8PathBuf,
    /// The host a `<host>:<path>` names: a directory exists on one
    /// machine only. `None` for a repository, and for a bare path, which
    /// is read on whichever host the agent is placed.
    pub host: Option<HostId>,
}

/// Resolves the Workdir field. `projects` are the desk's `(name, url)`
/// pairs. A project name resolves to its URL; a URL stands for itself;
/// `fern:/src/rho` is a path on host `fern`; anything else is a path on
/// the host the agent is placed on. Paths are the agent host's to expand
/// and validate, so the GUI never joins its own cwd or home.
pub fn resolve_workdir(
    projects: &[(String, Utf8PathBuf)],
    hosts: &Hosts,
    argument: &str,
) -> Result<Workdir, String> {
    if let Some((_, url)) = projects
        .iter()
        .find(|(name, url)| name == argument || url == argument)
    {
        return Ok(Workdir {
            path: url.clone(),
            host: None,
        });
    }
    // A Windows-style drive letter is not a thing on an agent host, so a
    // colon before any separator is a host prefix. A URL (`https://…`,
    // `git@host:path`) is what the agent host clones, not a host.
    if !is_repository_url(argument)
        && let Some((name, path)) = argument.split_once(':')
        && !name.contains('/')
    {
        let host = hosts
            .by_name(name)
            .ok_or_else(|| format!("no attached host named `{name}`"))?;
        return Ok(Workdir {
            path: Utf8PathBuf::from(path),
            host: Some(host.id),
        });
    }
    Ok(Workdir {
        path: Utf8PathBuf::from(argument),
        host: None,
    })
}

/// Where a new agent runs, in the order that decides it. What can only be
/// on one machine — the agent a start names, a directory a workdir names
/// with its host — settles it. Otherwise the user's choice in the draft,
/// then the default the caller derived.
#[derive(Clone, Copy, Debug, Default)]
pub struct Placement {
    /// Chosen in the draft (Shift-Tab in the Workdir field).
    pub chosen: Option<HostId>,
    /// The caller's default when nothing above settles it.
    pub default: Option<HostId>,
}

/// The host a new agent starts on and how it starts there, or the reason
/// it cannot. An agent target settles the host by itself: the new agent
/// shares that agent's repository, which only exists on that agent's
/// agent host. Where the workdir also names a host, the two must agree —
/// nothing downstream could reconcile a checkout on one machine with a
/// base revision on another.
pub fn parse_start(
    hosts: &Hosts,
    mode: StartFieldMode,
    target: &str,
    workdir: Option<Workdir>,
    placement: Placement,
    base: StartBase,
) -> Result<(HostId, Start), String> {
    let require_workdir = || {
        workdir.clone().ok_or_else(|| {
            "no repository for the new agent: type its URL in the \
             Workdir field, or register a project under space p a"
                .to_owned()
        })
    };
    // Here starts in the workdir alone: whatever the start field names is
    // not where it works.
    let base = match mode {
        StartFieldMode::Here => StartBase::default(),
        StartFieldMode::NewOn | StartFieldMode::Join => base,
    };
    let pinned = workdir.as_ref().and_then(|workdir| workdir.host);
    let settled = match (base.host, pinned) {
        (Some(base), Some(pinned)) if base != pinned => {
            return Err(format!(
                "`{target}` is on {}, but the working directory is on {}: \
                 an agent cannot start from a base on another host",
                hosts.host_label(base),
                hosts.host_label(pinned),
            ));
        }
        (base, pinned) => base.or(pinned),
    };
    let host = match (settled, placement.chosen) {
        (Some(settled), Some(chosen)) if settled != chosen => {
            return Err(format!(
                "this agent can only run on {}, not on the chosen host {}",
                hosts.host_label(settled),
                hosts.host_label(chosen),
            ));
        }
        (Some(host), _) | (None, Some(host)) => host,
        (None, None) => placement.default.ok_or_else(|| {
            if hosts.is_empty() {
                "not connected to an agent host".to_owned()
            } else {
                "which host? ctrl-tab in the Workdir field chooses one".to_owned()
            }
        })?,
    };
    let start = match (mode, target, base.agent) {
        (StartFieldMode::NewOn, "", _) => {
            return Err(
                "pick a base: a git revision like `origin/main` or an agent label".to_owned(),
            );
        }
        // An agent's change lives in its own clone, which a fresh clone
        // cannot see: work with it by joining it.
        (StartFieldMode::NewOn, _, Some(_)) => {
            return Err(format!(
                "`{target}` is an agent: Shift-Tab to Join mode to work in its directory, \
                 or base on a git revision like `origin/main`"
            ));
        }
        (StartFieldMode::NewOn, _, None) => {
            if target
                .strip_prefix('@')
                .is_some_and(|label| label.starts_with('a'))
            {
                return Err(format!("no agent named `{target}`"));
            }
            Start::NewOn {
                repo: require_workdir()?.path,
                rev: if target.eq_ignore_ascii_case(DEFAULT_START) {
                    AUTO_BASE_REV
                } else {
                    target
                }
                .to_owned(),
            }
        }
        (StartFieldMode::Join, _, Some((workset, cwd))) => Start::Join { workset, cwd },
        (StartFieldMode::Join, target, None) => {
            return Err(format!(
                "join target must be an agent label, not `{target}`"
            ));
        }
        (StartFieldMode::Here, _, _) => Start::Here {
            cwd: require_workdir()?.path,
        },
    };
    Ok((host, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Hosts {
        Hosts::new(std::sync::Arc::new(rho_agent_hosts::DroppedSink))
    }

    fn url(text: &str) -> Workdir {
        Workdir {
            path: Utf8PathBuf::from(text),
            host: None,
        }
    }

    /// A project is the desk's: its name gives its URL and no host, so
    /// the same project works wherever the agent is placed.
    #[test]
    fn a_project_resolves_to_its_url_and_no_host() {
        let projects = vec![(
            "rho".to_owned(),
            Utf8PathBuf::from("https://github.com/maan2003/rho"),
        )];
        assert_eq!(
            resolve_workdir(&projects, &hosts(), "rho").unwrap(),
            url("https://github.com/maan2003/rho")
        );
        // A URL nobody registered is still a URL, not a host prefix.
        assert_eq!(
            resolve_workdir(&[], &hosts(), "git@github.com:x/y").unwrap(),
            url("git@github.com:x/y")
        );
        assert!(
            resolve_workdir(&[], &hosts(), "fern:/src/rho")
                .unwrap_err()
                .contains("no attached host named `fern`")
        );
    }

    /// What settles the host outranks the default, and a choice that
    /// contradicts it is refused rather than silently overridden.
    #[test]
    fn placement_takes_the_agent_base_then_the_choice_then_the_default() {
        let base_on = |host: u32| StartBase {
            host: Some(HostId(host)),
            agent: None,
        };
        let start = |placement, base| {
            parse_start(
                &hosts(),
                StartFieldMode::NewOn,
                "origin/main",
                Some(url("https://x/y")),
                placement,
                base,
            )
            .map(|(host, _)| host)
        };
        let default = |host: u32| Placement {
            chosen: None,
            default: Some(HostId(host)),
        };
        assert_eq!(start(default(1), StartBase::default()), Ok(HostId(1)));
        assert_eq!(start(default(1), base_on(2)), Ok(HostId(2)));
        let chosen = Placement {
            chosen: Some(HostId(3)),
            default: Some(HostId(1)),
        };
        assert_eq!(start(chosen, StartBase::default()), Ok(HostId(3)));
        assert!(start(chosen, base_on(2)).is_err());
        assert_eq!(
            start(Placement::default(), StartBase::default()),
            Err("not connected to an agent host".to_owned())
        );
    }

    /// A base that names an agent on one host and a workdir on another is
    /// refused in words, not resolved to whichever came last.
    #[test]
    fn a_base_and_a_workdir_on_two_hosts_is_refused() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_agent_hosts::DroppedSink));
        let refusal = parse_start(
            &hosts,
            StartFieldMode::NewOn,
            "eng-1234",
            Some(Workdir {
                host: Some(HostId(2)),
                path: Utf8PathBuf::from("/src/rho"),
            }),
            Placement::default(),
            StartBase {
                host: Some(HostId(1)),
                agent: None,
            },
        )
        .expect_err("two hosts cannot both be right");
        assert!(refusal.contains("an agent cannot start from a base on another host"));
    }

    /// In place works in the workdir, on the workdir's host: whatever the
    /// start field still names, even an agent on another host, is not
    /// where it starts.
    #[test]
    fn in_place_starts_in_the_workdir_whatever_the_start_field_says() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_agent_hosts::DroppedSink));
        let (host, start) = parse_start(
            &hosts,
            StartFieldMode::Here,
            "eng-1234",
            Some(Workdir {
                host: Some(HostId(2)),
                path: Utf8PathBuf::from("/src/notes"),
            }),
            Placement::default(),
            StartBase {
                host: Some(HostId(1)),
                agent: None,
            },
        )
        .expect("a workdir is all in place needs");
        assert_eq!(host, HostId(2));
        assert_eq!(
            start,
            Start::Here {
                cwd: Utf8PathBuf::from("/src/notes")
            }
        );
    }

    /// The default start is a name, not a revision; what goes on the wire is
    /// the revision it stands for.
    #[test]
    fn the_default_base_goes_out_as_its_revision() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_agent_hosts::DroppedSink));
        let (host, start) = parse_start(
            &hosts,
            StartFieldMode::NewOn,
            DEFAULT_START,
            Some(Workdir {
                host: None,
                path: Utf8PathBuf::from("/src/rho"),
            }),
            Placement {
                chosen: None,
                default: Some(HostId(7)),
            },
            StartBase::default(),
        )
        .expect("a workdir and a default base are enough");
        assert_eq!(host, HostId(7));
        assert_eq!(
            start,
            Start::NewOn {
                repo: Utf8PathBuf::from("/src/rho"),
                rev: AUTO_BASE_REV.to_owned(),
            }
        );
    }

    /// No workdir and no base: the reader is told what to type, not that
    /// something went wrong.
    #[test]
    fn a_draft_with_no_workdir_says_what_to_type() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_agent_hosts::DroppedSink));
        let refusal = parse_start(
            &hosts,
            StartFieldMode::NewOn,
            DEFAULT_START,
            None,
            Placement {
                chosen: Some(HostId(1)),
                default: None,
            },
            StartBase::default(),
        )
        .expect_err("nothing says where the agent works");
        assert!(refusal.contains("type its URL in the Workdir field"));
    }

    /// Each start is the call it needs: a new workset of the right kind, or
    /// code in the workset the agent joins, and the message rides beside
    /// the code, never in it.
    #[test]
    fn each_start_is_the_call_it_needs() {
        let runtime = Runtime::parse("sol-high").unwrap();
        let message = Some(vec![ContentPart::Text {
            text: "print('pwned')".into(),
        }]);
        let workset: WorksetId = "0123456789ab".parse().unwrap();
        let joined = Start::Join {
            workset,
            cwd: "/src/rho".into(),
        }
        .creation(&runtime, message.clone());
        let Creation::Eval(eval) = joined else {
            panic!("joining is code in the workset joined: {joined:?}");
        };
        assert_eq!(eval.workset, workset);
        assert!(eval.code.contains("new_agent(cwd=\"/src/rho\""));
        assert_eq!(eval.message, message);
        assert!(!eval.code.contains("pwned"), "the message is not code");

        let Creation::Workset(here) = (Start::Here {
            cwd: "/home/me".into(),
        })
        .creation(&runtime, None) else {
            panic!("in place is a workset of its own");
        };
        assert_eq!(
            here.kind,
            WorksetKind::Host {
                cwd: "/home/me".into()
            }
        );
        let Creation::Workset(fresh) = (Start::NewOn {
            repo: "https://x/y".into(),
            rev: "main".into(),
        })
        .creation(&runtime, None) else {
            panic!("a new clone is a workset of its own");
        };
        assert_eq!(fresh.kind, WorksetKind::Isolated);
        assert!(fresh.code.contains("repo=\"https://x/y\", rev=\"main\""));
    }
}
