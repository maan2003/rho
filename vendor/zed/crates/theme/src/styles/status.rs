#![allow(missing_docs)]

use gpui::{Color, Hsla};
use refineable::Refineable;

use crate::{blue, grass, neutral, red, yellow};

#[derive(Refineable, Clone, Debug, PartialEq)]
#[refineable(Debug, serde::Deserialize)]
pub struct StatusColors {
    /// Indicates some kind of conflict, like a file changed on disk while it was open, or
    /// merge conflicts in a Git repository.
    pub conflict: Color,
    pub conflict_background: Color,
    pub conflict_border: Color,

    /// Indicates something new, like a new file added to a Git repository.
    pub created: Color,
    pub created_background: Color,
    pub created_border: Color,

    /// Indicates that something no longer exists, like a deleted file.
    pub deleted: Color,
    pub deleted_background: Color,
    pub deleted_border: Color,

    /// Indicates a system error, a failed operation or a diagnostic error.
    pub error: Color,
    pub error_background: Color,
    pub error_border: Color,

    /// Represents a hidden status, such as a file being hidden in a file tree.
    pub hidden: Color,
    pub hidden_background: Color,
    pub hidden_border: Color,

    /// Indicates a hint or some kind of additional information.
    pub hint: Color,
    pub hint_background: Color,
    pub hint_border: Color,

    /// Indicates that something is deliberately ignored, such as a file or operation ignored by Git.
    pub ignored: Color,
    pub ignored_background: Color,
    pub ignored_border: Color,

    /// Represents informational status updates or messages.
    pub info: Color,
    pub info_background: Color,
    pub info_border: Color,

    /// Indicates a changed or altered status, like a file that has been edited.
    pub modified: Color,
    pub modified_background: Color,
    pub modified_border: Color,

    /// Indicates something that is predicted, like automatic code completion, or generated code.
    pub predictive: Color,
    pub predictive_background: Color,
    pub predictive_border: Color,

    /// Represents a renamed status, such as a file that has been renamed.
    pub renamed: Color,
    pub renamed_background: Color,
    pub renamed_border: Color,

    /// Indicates a successful operation or task completion.
    pub success: Color,
    pub success_background: Color,
    pub success_border: Color,

    /// Indicates some kind of unreachable status, like a block of code that can never be reached.
    pub unreachable: Color,
    pub unreachable_background: Color,
    pub unreachable_border: Color,

    /// Represents a warning status, like an operation that is about to fail.
    pub warning: Color,
    pub warning_background: Color,
    pub warning_border: Color,
}

pub struct DiagnosticColors {
    pub error: Hsla,
    pub warning: Hsla,
    pub info: Hsla,
}

impl StatusColors {
    pub fn dark() -> Self {
        Self {
            conflict: red().dark().step_9(),
            conflict_background: red().dark().step_9(),
            conflict_border: red().dark().step_9(),
            created: grass().dark().step_9(),
            created_background: grass().dark().step_9().opacity(0.25),
            created_border: grass().dark().step_9(),
            deleted: red().dark().step_9(),
            deleted_background: red().dark().step_9().opacity(0.25),
            deleted_border: red().dark().step_9(),
            error: red().dark().step_9(),
            error_background: red().dark().step_9(),
            error_border: red().dark().step_9(),
            hidden: neutral().dark().step_9(),
            hidden_background: neutral().dark().step_9(),
            hidden_border: neutral().dark().step_9(),
            hint: blue().dark().step_9(),
            hint_background: blue().dark().step_9(),
            hint_border: blue().dark().step_9(),
            ignored: neutral().dark().step_9(),
            ignored_background: neutral().dark().step_9(),
            ignored_border: neutral().dark().step_9(),
            info: blue().dark().step_9(),
            info_background: blue().dark().step_9(),
            info_border: blue().dark().step_9(),
            modified: yellow().dark().step_9(),
            modified_background: yellow().dark().step_9().opacity(0.25),
            modified_border: yellow().dark().step_9(),
            predictive: neutral().dark_alpha().step_9(),
            predictive_background: neutral().dark_alpha().step_9(),
            predictive_border: neutral().dark_alpha().step_9(),
            renamed: blue().dark().step_9(),
            renamed_background: blue().dark().step_9(),
            renamed_border: blue().dark().step_9(),
            success: grass().dark().step_9(),
            success_background: grass().dark().step_9(),
            success_border: grass().dark().step_9(),
            unreachable: neutral().dark().step_10(),
            unreachable_background: neutral().dark().step_10(),
            unreachable_border: neutral().dark().step_10(),
            warning: yellow().dark().step_9(),
            warning_background: yellow().dark().step_9(),
            warning_border: yellow().dark().step_9(),
        }
    }

    pub fn light() -> Self {
        Self {
            conflict: red().light().step_9(),
            conflict_background: red().light().step_9(),
            conflict_border: red().light().step_9(),
            created: grass().light().step_9(),
            created_background: grass().light().step_9(),
            created_border: grass().light().step_9(),
            deleted: red().light().step_9(),
            deleted_background: red().light().step_9(),
            deleted_border: red().light().step_9(),
            error: red().light().step_9(),
            error_background: red().light().step_9(),
            error_border: red().light().step_9(),
            hidden: neutral().light().step_9(),
            hidden_background: neutral().light().step_9(),
            hidden_border: neutral().light().step_9(),
            hint: blue().light().step_9(),
            hint_background: blue().light().step_9(),
            hint_border: blue().light().step_9(),
            ignored: neutral().light().step_9(),
            ignored_background: neutral().light().step_9(),
            ignored_border: neutral().light().step_9(),
            info: blue().light().step_9(),
            info_background: blue().light().step_9(),
            info_border: blue().light().step_9(),
            modified: yellow().light().step_9(),
            modified_background: yellow().light().step_9(),
            modified_border: yellow().light().step_9(),
            predictive: neutral().light_alpha().step_9(),
            predictive_background: neutral().light_alpha().step_9(),
            predictive_border: neutral().light_alpha().step_9(),
            renamed: blue().light().step_9(),
            renamed_background: blue().light().step_9(),
            renamed_border: blue().light().step_9(),
            success: grass().light().step_9(),
            success_background: grass().light().step_9(),
            success_border: grass().light().step_9(),
            unreachable: neutral().light().step_10(),
            unreachable_background: neutral().light().step_10(),
            unreachable_border: neutral().light().step_10(),
            warning: yellow().light().step_9(),
            warning_background: yellow().light().step_9(),
            warning_border: yellow().light().step_9(),
        }
    }

    pub fn diagnostic(&self) -> DiagnosticColors {
        DiagnosticColors {
            error: self.error.into(),
            warning: self.warning.into(),
            info: self.info.into(),
        }
    }
}
