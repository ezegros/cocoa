//! OSC 7501 reports for Cocoa's root record, independent of terminal rendering.

use std::io::{self, Write};

use base64::{Engine, prelude::BASE64_STANDARD};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Status {
    #[default]
    Idle,
    Working(&'static str),
    Done,
    Blocked {
        kind: Kind,
        message: &'static str,
    },
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Permission,
    Question,
    Auth,
}

impl Status {
    pub fn report(self) -> io::Result<()> {
        let (state, kind, message) = match self {
            Self::Idle => ("idle", None, "Ready"),
            Self::Working(message) => ("working", None, message),
            Self::Done => ("done", None, "Response ready"),
            Self::Blocked { kind, message } => ("blocked", Some(kind), message),
            Self::Error => ("error", None, "Cocoa failed; see terminal for details"),
        };
        assert!(
            message.len() <= 2048 && !message.chars().any(char::is_control),
            "program status messages must fit OSC 7501's single-line text limits"
        );
        let mut report = format!("\x1b]7501;state={state}:app=cocoa");
        if let Some(kind) = kind {
            report.push_str(match kind {
                Kind::Permission => ":kind=permission",
                Kind::Question => ":kind=question",
                Kind::Auth => ":kind=auth",
            });
        }
        report.push_str(":msg=");
        BASE64_STANDARD.encode_string(message, &mut report);
        report.push_str("\x1b\\");
        let mut output = io::stdout().lock();
        output.write_all(report.as_bytes())?;
        output.flush()
    }
}
