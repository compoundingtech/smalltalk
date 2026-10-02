//! How each kind of conversation entry is drawn, by theme token: the one table the renderer
//! reads. stui writes it to `fixtures/clients/conversation-style.json` and the phone draws
//! from that file, so both apps draw a conversation the same way.

use crate::theme::Theme;
use ratatui::style::Color;
use serde::Serialize;

/// A colour named by its theme token (`fixtures/clients/theme.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Token {
    Text,
    Subtext0,
    Overlay0,
    Overlay1,
    Surface1,
    Surface2,
    Working,
    Green,
    Red,
    Sapphire,
    Peach,
    UserBg,
    ToolBg,
}

impl Token {
    pub fn color(self, theme: &Theme) -> Color {
        match self {
            Token::Text => theme.text,
            Token::Subtext0 => theme.subtext0,
            Token::Overlay0 => theme.overlay0,
            Token::Overlay1 => theme.overlay1,
            Token::Surface1 => theme.surface1,
            Token::Surface2 => theme.surface2,
            Token::Working => theme.working,
            Token::Green => theme.green,
            Token::Red => theme.red,
            Token::Sapphire => theme.sapphire,
            Token::Peach => theme.peach,
            Token::UserBg => theme.user_bg,
            Token::ToolBg => theme.tool_bg,
        }
    }
}

/// Words drawn in a colour.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Label {
    pub text: &'static str,
    pub color: Token,
}

/// The person's own messages: a filled block.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct User {
    pub fill: Token,
    pub text: Token,
    pub time: Token,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Thinking {
    pub mark: &'static str,
    pub color: Token,
    pub italic: bool,
}

/// One look of a tool call's title and output rows.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ToolLook {
    pub title: Token,
    pub title_bold: bool,
    pub rows: Token,
}

/// Tool calls: an edge in the outcome's colour and no fill; quiet until opened.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Tool {
    pub fill: Option<Token>,
    pub running: Token,
    pub ok: Label,
    pub failed: Label,
    pub quiet: ToolLook,
    pub open: ToolLook,
    /// Output rows that start with `+` or `-` (or say "error").
    pub added: Token,
    pub removed: Token,
    /// The most rows a call shows until opened, wrapped rows counted; failed calls fold too.
    pub collapsed_rows: usize,
    pub collapse: Label,
}

/// One side of Small Talk mail: who sent it, its edge and its text.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MailLook {
    pub edge: Token,
    pub from: Token,
    pub from_bold: bool,
    pub text: Token,
}

/// Small Talk mail: mail the person is part of leads, mail between others stays back, and
/// mail to the person is filled like their own messages.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Mail {
    pub involving_you: MailLook,
    pub between_others: MailLook,
    pub to_you_fill: Token,
    /// The person's own mail: st has it, then the agent's harness has it.
    pub sent: Label,
    pub delivered: Label,
}

/// A message the person sent that st has not confirmed yet.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Pending {
    pub sending: Label,
    pub sending_edge: Token,
    pub failed: Token,
    pub unconfirmed: Token,
    pub text: Token,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Event {
    pub rule: Token,
    pub label: Token,
}

/// The simplified conversation's tool calls: one line each, and a run of them one line that
/// says how many and how they went ("▸ 7 tool calls · ✓6 ✕1 · last: $ cargo test").
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Bundle {
    /// Marks a folded run, then an opened one.
    pub folded: &'static str,
    pub opened: &'static str,
    pub label: Token,
    /// "last:" and the last call's title.
    pub last: Token,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Rules {
    pub user: User,
    pub assistant: Token,
    pub thinking: Thinking,
    pub tool: Tool,
    pub mail: Mail,
    pub pending: Pending,
    pub event: Event,
    pub bundle: Bundle,
}

pub const RULES: Rules = Rules {
    user: User {
        fill: Token::UserBg,
        text: Token::Text,
        time: Token::Overlay0,
    },
    assistant: Token::Text,
    thinking: Thinking {
        mark: "∴",
        color: Token::Overlay1,
        italic: true,
    },
    tool: Tool {
        fill: None,
        running: Token::Working,
        ok: Label {
            text: "✓",
            color: Token::Green,
        },
        failed: Label {
            text: "✕",
            color: Token::Red,
        },
        quiet: ToolLook {
            title: Token::Overlay1,
            title_bold: false,
            rows: Token::Overlay0,
        },
        open: ToolLook {
            title: Token::Text,
            title_bold: true,
            rows: Token::Subtext0,
        },
        added: Token::Green,
        removed: Token::Red,
        collapsed_rows: 6,
        collapse: Label {
            text: "▴ collapse",
            color: Token::Overlay0,
        },
    },
    mail: Mail {
        involving_you: MailLook {
            edge: Token::Sapphire,
            from: Token::Sapphire,
            from_bold: true,
            text: Token::Text,
        },
        between_others: MailLook {
            edge: Token::Surface2,
            from: Token::Overlay1,
            from_bold: false,
            text: Token::Overlay1,
        },
        to_you_fill: Token::ToolBg,
        sent: Label {
            text: "✓ sent",
            color: Token::Overlay0,
        },
        delivered: Label {
            text: "✓✓ delivered",
            color: Token::Green,
        },
    },
    pending: Pending {
        sending: Label {
            text: "you · sending…",
            color: Token::Overlay0,
        },
        sending_edge: Token::Surface2,
        failed: Token::Red,
        unconfirmed: Token::Peach,
        text: Token::Overlay0,
    },
    event: Event {
        rule: Token::Surface1,
        label: Token::Overlay0,
    },
    bundle: Bundle {
        folded: "▸",
        opened: "▾",
        label: Token::Overlay1,
        last: Token::Overlay0,
    },
};
