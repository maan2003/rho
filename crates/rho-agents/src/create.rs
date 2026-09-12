//! Making an agent: what the draft means, and what it takes to start one.
//!
//! A draft is four answers — a working directory, a base, a role and a
//! first message — and every way one of them can be refused is here, in
//! the words the reader is shown. What the shell keeps is the screen and
//! the buffers; what it asks of this module is whether the answers make a
//! `NewAgent`, and on which host.

use camino::Utf8PathBuf;
use rho_hosts::{HostId, HostPath, Hosts};
use rho_ui_proto::{AgentRole, EngineerIntelligence, JoinTarget, StartMode, WorkspaceInfo};

/// The user-facing name for selecting the first available conventional base.
pub const DEFAULT_START: &str = "auto";
/// The git revision represented by [`DEFAULT_START`]: none, so the agent
/// starts where a fresh clone is born, on the remote's default branch.
pub const AUTO_BASE_REV: &str = "";
pub const DEFAULT_ROLE: &str = "eng";

/// How the start field's target is interpreted; cycled with Shift-Tab while
/// the cursor is in the field. The field label shows the current mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFieldMode {
    /// A fresh clone with the target revision checked out.
    NewOn,
    /// The same directory as the target agent.
    Join,
}

/// What the agents map says about the label in the start field: which host
/// that agent is on, and the workspace it works in. Both are the map's to
/// answer, so the caller looks them up and hands them in.
#[derive(Clone, Debug, Default)]
pub struct StartBase {
    pub host: Option<HostId>,
    pub workspace: Option<WorkspaceInfo>,
}

/// Resolves a workdir argument to a directory on a specific daemon. A
/// registered project name resolves to its registration; anything else is
/// a raw daemon-side path, which may name its host as `fern:/src/rho`.
/// Paths name directories on the daemon's machine, so the GUI never joins
/// its own cwd or expands its own home — the daemon expands `~` and
/// validates.
pub fn resolve_workdir(hosts: &Hosts, argument: &str) -> Result<HostPath, String> {
    if let Some(registered) = hosts.registered_workdir(argument) {
        return Ok(registered);
    }
    // A Windows-style drive letter is not a thing on a daemon host, so a
    // colon before any separator is unambiguously a host prefix. A URL
    // (`https://…`, `git@host:path`) is what the daemon clones, not a host.
    let is_url = argument.contains("://") || argument.starts_with("git@");
    if !is_url
        && let Some((name, path)) = argument.split_once(':')
        && !name.contains('/')
    {
        let host = hosts
            .by_name(name)
            .ok_or_else(|| format!("no attached host named `{name}`"))?;
        return Ok(HostPath {
            host: host.id,
            path: Utf8PathBuf::from(path),
        });
    }
    let host = match hosts.len() {
        0 => return Err("not connected to rho-daemon".to_owned()),
        1 => hosts.iter().next().expect("one host").id,
        _ => {
            return Err(format!(
                "`{argument}` does not say which host: write `<host>:{argument}` \
                 or use a registered project name"
            ));
        }
    };
    Ok(HostPath {
        host,
        path: Utf8PathBuf::from(argument),
    })
}

/// The host a new agent starts on and how it starts there, or the reason
/// it cannot. An agent target settles the host by itself: the new agent
/// shares that agent's repository, which only exists on that agent's
/// daemon. Where the workdir also names a host, the two must agree —
/// nothing downstream could reconcile a checkout on one machine with a
/// base revision on another.
pub fn parse_start(
    hosts: &Hosts,
    mode: StartFieldMode,
    target: &str,
    workdir: Option<HostPath>,
    selected_host: Option<HostId>,
    base: StartBase,
) -> Result<(HostId, StartMode), String> {
    let require_workdir = || {
        workdir.clone().ok_or_else(|| {
            "no repository for the new agent: type its URL in the \
             Workdir field, or register a project under space p a"
                .to_owned()
        })
    };
    let base_host = base.host;
    if let Some(selected) = selected_host {
        if let Some(workdir) = &workdir
            && workdir.host != selected
        {
            return Err("the selected project belongs to a different host".to_owned());
        }
        if let Some(base) = base_host
            && base != selected
        {
            return Err(format!(
                "`{target}` is on {}, not the selected host {}",
                hosts.host_label(base),
                hosts.host_label(selected),
            ));
        }
    }
    let host = match (base_host, &workdir) {
        (Some(base), Some(workdir)) if base != workdir.host => {
            return Err(format!(
                "`{target}` is on {}, but the working directory is on {}: \
                 an agent cannot start from a base on another host",
                hosts.host_label(base),
                hosts.host_label(workdir.host),
            ));
        }
        (Some(base), _) => base,
        (None, Some(workdir)) => workdir.host,
        (None, None) => selected_host
            .or_else(|| hosts.primary())
            .ok_or_else(|| "not connected to rho-daemon".to_owned())?,
    };
    let workspace = base.workspace;
    let start = match (mode, target, workspace) {
        (StartFieldMode::NewOn, "", _) => {
            return Err(
                "pick a base: a git revision like `origin/main` or an agent label".to_owned(),
            );
        }
        // An agent's change lives in its own clone, which a fresh clone
        // cannot see: work with it by joining it.
        (StartFieldMode::NewOn, _, Some(WorkspaceInfo::Workset { .. })) => {
            return Err(format!(
                "`{target}` is an agent: Shift-Tab to Join mode to work in its directory, \
                 or base on a git revision like `origin/main`"
            ));
        }
        (
            StartFieldMode::NewOn,
            _,
            Some(WorkspaceInfo::Workspace { repo, id } | WorkspaceInfo::Sandbox { repo, id }),
        ) => StartMode::NewOn {
            repo,
            revset: format!("{}@", id.encoded()),
        },
        // An agent in the user's checkout works on the user's own change.
        (StartFieldMode::NewOn, _, Some(WorkspaceInfo::UserCheckout { repo })) => {
            StartMode::NewOn {
                repo,
                revset: "@".to_owned(),
            }
        }
        (StartFieldMode::NewOn, _, None) => {
            if target.eq_ignore_ascii_case("user") {
                return Err(
                    "`user` is a join target; base on a git revision like `origin/main`, \
                     or Shift-Tab to Join mode"
                        .to_owned(),
                );
            }
            if target
                .strip_prefix('@')
                .is_some_and(|label| label.starts_with('a'))
            {
                return Err(format!("no agent named `{target}`"));
            }
            StartMode::NewOn {
                repo: require_workdir()?.path,
                revset: if target.eq_ignore_ascii_case(DEFAULT_START) {
                    AUTO_BASE_REV
                } else {
                    target
                }
                .to_owned(),
            }
        }
        (StartFieldMode::Join, _, Some(workspace)) => {
            StartMode::Join(JoinTarget::Workspace(workspace))
        }
        (StartFieldMode::Join, target, None) => {
            if target.is_empty() || target.eq_ignore_ascii_case("user") {
                StartMode::Join(JoinTarget::User {
                    repo: require_workdir()?.path,
                })
            } else {
                return Err(format!(
                    "join target must be `user` or an agent label, not `{target}`"
                ));
            }
        }
    };
    Ok((host, start))
}

pub fn parse_agent_role(text: &str) -> Result<AgentRole, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "" | "eng" => Ok(AgentRole::default()),
        "eng-mini" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        }),
        "eng-low" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        }),
        "eng-cheap" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        }),
        "eng-high" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        }),
        "eng-ultra" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        }),
        "eng-alt" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Alt,
        }),
        "eng-gemini" => Ok(AgentRole::Engineer {
            intelligence: EngineerIntelligence::Gemini,
        }),
        other => Err(format!(
            "unknown role `{other}`; use eng, eng-mini, eng-low, eng-cheap, eng-high, eng-ultra, eng-alt, or eng-gemini"
        )),
    }
}

pub fn cycle_agent_role_text(current: &str) -> &'static str {
    match parse_agent_role(current).unwrap_or_default() {
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
            ..
        } => "eng-low",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
            ..
        } => "eng-cheap",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
            ..
        } => "eng",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
            ..
        } => "eng-high",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
            ..
        } => "eng-ultra",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
            ..
        } => "eng-alt",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Alt,
            ..
        } => "eng-gemini",
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Gemini,
            ..
        } => "pm",
        AgentRole::Advisor { .. } => "eng",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_role() {
        assert!(parse_agent_role("eng-py").is_err());
        assert!(parse_agent_role("eng-ultra-py").is_err());
        assert_eq!(cycle_agent_role_text("eng"), "eng-high");
        assert_eq!(cycle_agent_role_text("eng-ultra"), "eng-alt");

        assert_eq!(
            parse_agent_role("eng-low").unwrap(),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Low,
            }
        );
        assert_eq!(
            parse_agent_role("eng-gemini").unwrap(),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Gemini,
            }
        );
        assert!(parse_agent_role("pm ultra").is_err());
        assert!(parse_agent_role("eng-ultra-fast").is_err());
        assert!(parse_agent_role("advisor high").is_err());
    }

    /// A base that names an agent on one host and a workdir on another is
    /// refused in words, not resolved to whichever came last.
    #[test]
    fn a_base_and_a_workdir_on_two_hosts_is_refused() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_hosts::DroppedSink));
        let refusal = parse_start(
            &hosts,
            StartFieldMode::NewOn,
            "eng-1234",
            Some(HostPath {
                host: HostId(2),
                path: Utf8PathBuf::from("/src/rho"),
            }),
            None,
            StartBase {
                host: Some(HostId(1)),
                workspace: None,
            },
        )
        .expect_err("two hosts cannot both be right");
        assert!(refusal.contains("an agent cannot start from a base on another host"));
    }

    /// The default start is a name, not a revision; what goes on the wire is
    /// the revision it stands for.
    #[test]
    fn the_default_base_goes_out_as_its_revision() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_hosts::DroppedSink));
        let (host, start) = parse_start(
            &hosts,
            StartFieldMode::NewOn,
            DEFAULT_START,
            Some(HostPath {
                host: HostId(7),
                path: Utf8PathBuf::from("/src/rho"),
            }),
            None,
            StartBase::default(),
        )
        .expect("a workdir and a default base are enough");
        assert_eq!(host, HostId(7));
        assert_eq!(
            start,
            StartMode::NewOn {
                repo: Utf8PathBuf::from("/src/rho"),
                revset: AUTO_BASE_REV.to_owned(),
            }
        );
    }

    /// No workdir and no base: the reader is told what to type, not that
    /// something went wrong.
    #[test]
    fn a_draft_with_no_workdir_says_what_to_type() {
        let hosts = Hosts::new(std::sync::Arc::new(rho_hosts::DroppedSink));
        let refusal = parse_start(
            &hosts,
            StartFieldMode::NewOn,
            DEFAULT_START,
            None,
            Some(HostId(1)),
            StartBase::default(),
        )
        .expect_err("nothing says where the agent works");
        assert!(refusal.contains("type its URL in the Workdir field"));
    }
}
