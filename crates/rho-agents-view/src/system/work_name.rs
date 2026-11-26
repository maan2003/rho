//! What the work is called on every screen and prompt about it
//! (`agents.An agent is labour, not a colleague`): `<project> · <thread
//! subject>`, read from the thread the work is for. An agent's short code
//! follows only where it tells two agents of one workset apart.

/// What one agent's work is called, as the chat log and the directory say
/// it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkName {
    /// The project's address, `tide` or `tide@lab`; none for work in no
    /// project, or before the chat log is read.
    pub project: Option<String>,
    /// The subject of the thread the work is for; none until the log says.
    pub subject: Option<String>,
    /// The agent's short code, `eng-q9tz`.
    pub code: String,
    /// Another agent works in the same workset.
    pub shared: bool,
}

impl WorkName {
    /// The agent: its work, with its code after it in a shared workset.
    pub fn agent(&self) -> String {
        let mut label = self.work();
        if self.shared && self.subject.is_some() {
            label.push_str(" · ");
            label.push_str(&self.code);
        }
        label
    }

    /// The workset, and its tools: the work alone, never an agent's code
    /// while the subject is known.
    pub fn work(&self) -> String {
        match (&self.project, &self.subject) {
            (Some(project), Some(subject)) => format!("{project} · {subject}"),
            (None, Some(subject)) => subject.clone(),
            // The subject is not known yet: the code stands in for it.
            (Some(project), None) => format!("{project} · {}", self.code),
            (None, None) => self.code.clone(),
        }
    }
}

/// A tool of the workset whose work is `work`: `tide · Flaky clock test ·
/// term 0`, `… · shell`, `… · desktop browser`.
pub(super) fn tool_title(work: &str, part: &super::Part) -> String {
    format!("{work} · {part}")
}

/// A transcript's title: what it is called, its host, and the folder and
/// runtime of the agent it shows, each only when known.
pub(super) fn transcript_title(
    label: &str,
    host: &str,
    folder: Option<&str>,
    runtime: Option<&str>,
) -> String {
    std::iter::once(label)
        .chain(Some(host).filter(|host| !host.is_empty()))
        .chain(folder)
        .chain(runtime)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Gives each of `labels` that another shares its `code`, so a prompt
/// never offers two answers that read the same.
pub(super) fn distinct(labels: &mut [(String, String)]) {
    let repeated: Vec<bool> = labels
        .iter()
        .map(|(label, _)| labels.iter().filter(|(other, _)| other == label).count() > 1)
        .collect();
    for ((label, code), repeated) in labels.iter_mut().zip(repeated) {
        if repeated && !label.ends_with(code.as_str()) {
            label.push_str(" · ");
            label.push_str(code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(project: Option<&str>, subject: Option<&str>, shared: bool) -> WorkName {
        WorkName {
            project: project.map(str::to_owned),
            subject: subject.map(str::to_owned),
            code: "eng-q9tz".into(),
            shared,
        }
    }

    #[test]
    fn one_agent_is_its_work_and_never_its_code() {
        let alone = name(Some("tide"), Some("Flaky clock test"), false);
        assert_eq!(alone.agent(), "tide · Flaky clock test");
        assert_eq!(alone.work(), "tide · Flaky clock test");
    }

    #[test]
    fn a_shared_workset_tells_its_agents_apart_but_not_its_tools() {
        let shared = name(Some("tide"), Some("Flaky clock test"), true);
        assert_eq!(shared.agent(), "tide · Flaky clock test · eng-q9tz");
        assert_eq!(shared.work(), "tide · Flaky clock test");
    }

    #[test]
    fn the_code_stands_in_for_a_subject_not_known_yet() {
        for shared in [false, true] {
            let early = name(Some("tide"), None, shared);
            assert_eq!(early.agent(), "tide · eng-q9tz", "shared: {shared}");
            assert_eq!(early.work(), "tide · eng-q9tz");
            assert_eq!(name(None, None, shared).agent(), "eng-q9tz");
        }
        assert_eq!(
            name(None, Some("Look at logs"), false).agent(),
            "Look at logs"
        );
    }

    #[test]
    fn tools_and_transcripts_are_named_by_the_work() {
        use super::super::Part;
        for shared in [false, true] {
            let work = name(Some("tide"), Some("Flaky clock test"), shared).work();
            assert_eq!(
                tool_title(&work, &Part::Terminal(0)),
                "tide · Flaky clock test · term 0"
            );
            assert_eq!(
                tool_title(&work, &Part::Shell),
                "tide · Flaky clock test · shell"
            );
            assert_eq!(
                tool_title(&work, &Part::Desktop("browser".into())),
                "tide · Flaky clock test · desktop browser"
            );
        }
        let alone = name(Some("tide"), Some("Flaky clock test"), false);
        assert_eq!(
            transcript_title(&alone.work(), "lab", Some("~/src/tide"), Some("opus-high")),
            "tide · Flaky clock test · lab · ~/src/tide · opus-high"
        );
        // Narrowed to one of two agents, its code follows the work.
        let narrowed = name(Some("tide"), Some("Flaky clock test"), true);
        assert_eq!(
            transcript_title(&narrowed.agent(), "lab", None, Some("sol-low")),
            "tide · Flaky clock test · eng-q9tz · lab · sol-low"
        );
        assert_eq!(
            transcript_title("tide · eng-q9tz", "", None, None),
            "tide · eng-q9tz"
        );
    }

    #[test]
    fn repeated_answers_get_their_codes() {
        let mut labels = vec![
            ("tide · Flaky clock test".to_owned(), "eng-aaaa".to_owned()),
            ("tide · Flaky clock test".to_owned(), "eng-bbbb".to_owned()),
            ("reef · Docs".to_owned(), "eng-cccc".to_owned()),
            (
                "tide · Flaky clock test · eng-dddd".to_owned(),
                "eng-dddd".to_owned(),
            ),
        ];
        distinct(&mut labels);
        let labels: Vec<_> = labels.into_iter().map(|(label, _)| label).collect();
        assert_eq!(
            labels,
            [
                "tide · Flaky clock test · eng-aaaa",
                "tide · Flaky clock test · eng-bbbb",
                "reef · Docs",
                "tide · Flaky clock test · eng-dddd",
            ]
        );
    }
}
