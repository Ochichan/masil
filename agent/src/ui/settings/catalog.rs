//! Settings shown by `rmux-agent settings`. Each entry saves one `@rmux-*`
//! choice that the built-in UI layer (core/rmux-ui-layer.conf) reads.

use crate::ui::model::Language;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Group {
    Bar,
    Panes,
    Input,
    Look,
    Layer,
}

impl Group {
    pub(crate) const ALL: [Self; 5] =
        [Self::Bar, Self::Panes, Self::Input, Self::Look, Self::Layer];

    pub(crate) fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Bar, Language::English) => "Status bar",
            (Self::Bar, Language::Korean) => "상태줄",
            (Self::Panes, Language::English) => "Panes",
            (Self::Panes, Language::Korean) => "창 분할",
            (Self::Input, Language::English) => "Mouse and input",
            (Self::Input, Language::Korean) => "마우스와 입력",
            (Self::Look, Language::English) => "Appearance",
            (Self::Look, Language::Korean) => "모양",
            (Self::Layer, Language::English) => "rmux UI",
            (Self::Layer, Language::Korean) => "rmux UI",
        }
    }
}

/// Where a tmux option lives, for reading back its effective value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Scope {
    Session,
    Window,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Choice {
    pub value: &'static str,
    pub en: &'static str,
    pub ko: &'static str,
}

impl Choice {
    pub(crate) fn label(&self, language: Language) -> &'static str {
        match language {
            Language::English => self.en,
            Language::Korean => self.ko,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Choices(&'static [Choice]),
    Number { min: u32, max: u32, step: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Setting {
    /// The saved `@rmux-*` option.
    pub key: &'static str,
    pub default: &'static str,
    pub kind: Kind,
    pub group: Group,
    /// Layer sections to run again after the choice changes.
    pub sections: &'static [&'static str],
    /// The tmux option that shows the effect, when there is one.
    pub readback: Option<(&'static str, Scope)>,
    pub en: &'static str,
    pub ko: &'static str,
    pub help_en: &'static str,
    pub help_ko: &'static str,
}

impl Setting {
    pub(crate) fn label(&self, language: Language) -> &'static str {
        match language {
            Language::English => self.en,
            Language::Korean => self.ko,
        }
    }

    pub(crate) fn help(&self, language: Language) -> &'static str {
        match language {
            Language::English => self.help_en,
            Language::Korean => self.help_ko,
        }
    }

    pub(crate) fn accepts(&self, value: &str) -> bool {
        match self.kind {
            Kind::Choices(choices) => choices.iter().any(|choice| choice.value == value),
            Kind::Number { min, max, step } => value
                .parse::<u32>()
                .is_ok_and(|number| (min..=max).contains(&number) && (number - min) % step == 0),
        }
    }
}

const fn choice(value: &'static str, en: &'static str, ko: &'static str) -> Choice {
    Choice { value, en, ko }
}

const ON_OFF: &[Choice] = &[choice("on", "On", "켜기"), choice("off", "Off", "끄기")];

pub(crate) const UI_KEY: &str = "@rmux-ui";

pub(crate) const SETTINGS: &[Setting] = &[
    Setting {
        key: "@rmux-status-position",
        default: "bottom",
        kind: Kind::Choices(&[
            choice("top", "Top", "위"),
            choice("bottom", "Bottom", "아래"),
            choice("left", "Left sidebar", "왼쪽 사이드바"),
            choice("right", "Right sidebar", "오른쪽 사이드바"),
        ]),
        group: Group::Bar,
        sections: &["base"],
        readback: Some(("status-position", Scope::Session)),
        en: "Position",
        ko: "위치",
        help_en: "Left and right draw the bar as a sidebar beside every window.",
        help_ko: "왼쪽과 오른쪽은 모든 창 옆에 상태줄을 사이드바로 그립니다.",
    },
    Setting {
        key: "@rmux-status",
        default: "on",
        kind: Kind::Choices(ON_OFF),
        group: Group::Bar,
        sections: &["base"],
        readback: Some(("status", Scope::Session)),
        en: "Show the bar",
        ko: "상태줄 표시",
        help_en: "Off hides the bar and its buttons; right-click menus still work.",
        help_ko: "끄면 상태줄과 버튼이 사라집니다. 오른쪽 클릭 메뉴는 계속 쓸 수 있습니다.",
    },
    Setting {
        key: "@rmux-status-width",
        default: "24",
        kind: Kind::Number {
            min: 12,
            max: 48,
            step: 2,
        },
        group: Group::Bar,
        sections: &[],
        readback: Some(("@rmux-status-width", Scope::Session)),
        en: "Sidebar width",
        ko: "사이드바 너비",
        help_en: "Columns used by a left or right bar.",
        help_ko: "왼쪽·오른쪽 상태줄이 차지하는 열 수입니다.",
    },
    Setting {
        key: "@rmux-clock",
        default: "on",
        kind: Kind::Choices(ON_OFF),
        group: Group::Bar,
        sections: &[],
        readback: None,
        en: "Clock",
        ko: "시계",
        help_en: "Shows the time beside the Settings button.",
        help_ko: "설정 버튼 옆에 시각을 표시합니다.",
    },
    Setting {
        key: "@rmux-pane-titles",
        default: "top",
        kind: Kind::Choices(&[
            choice("off", "Off", "끄기"),
            choice("top", "Top", "위"),
            choice("bottom", "Bottom", "아래"),
        ]),
        group: Group::Panes,
        sections: &["panes"],
        readback: Some(("pane-border-status", Scope::Window)),
        en: "Pane titles",
        ko: "pane 제목줄",
        help_en: "Click a title to rename the pane; float, zoom and x act on it.",
        help_ko: "제목을 누르면 이름을 바꿉니다. float, zoom, x 버튼도 제목줄에 있습니다.",
    },
    Setting {
        key: "@rmux-scrollbars",
        default: "modal",
        kind: Kind::Choices(&[
            choice("off", "Off", "끄기"),
            choice("modal", "While scrolling", "스크롤할 때"),
            choice("on", "Always", "항상"),
        ]),
        group: Group::Panes,
        sections: &["panes"],
        readback: Some(("pane-scrollbars", Scope::Window)),
        en: "Scrollbars",
        ko: "스크롤바",
        help_en: "Drag a scrollbar to move through the pane's history.",
        help_ko: "스크롤바를 끌어 pane 기록을 이동합니다.",
    },
    Setting {
        key: "@rmux-border-lines",
        default: "single",
        kind: Kind::Choices(&[
            choice("single", "Single", "한 줄"),
            choice("rounded", "Rounded", "둥근 모서리"),
            choice("heavy", "Heavy", "굵게"),
            choice("double", "Double", "두 줄"),
        ]),
        group: Group::Panes,
        sections: &["panes"],
        readback: Some(("pane-border-lines", Scope::Window)),
        en: "Borders",
        ko: "경계선",
        help_en: "Line style between panes.",
        help_ko: "pane 사이 경계선 모양입니다.",
    },
    Setting {
        key: "@rmux-mouse",
        default: "on",
        kind: Kind::Choices(ON_OFF),
        group: Group::Input,
        sections: &["base"],
        readback: Some(("mouse", Scope::Session)),
        en: "Mouse",
        ko: "마우스",
        help_en: "Off gives clicks to the terminal; rmux buttons and menus stop responding.",
        help_ko: "끄면 클릭이 터미널로 갑니다. rmux 버튼과 메뉴는 반응하지 않습니다.",
    },
    Setting {
        key: "@rmux-base-index",
        default: "0",
        kind: Kind::Choices(&[choice("0", "0", "0"), choice("1", "1", "1")]),
        group: Group::Input,
        sections: &["base"],
        readback: Some(("base-index", Scope::Session)),
        en: "Numbers start at",
        ko: "번호 시작",
        help_en: "First number for new windows and panes.",
        help_ko: "새 창과 pane의 첫 번호입니다.",
    },
    Setting {
        key: "@rmux-renumber",
        default: "off",
        kind: Kind::Choices(ON_OFF),
        group: Group::Input,
        sections: &["base"],
        readback: Some(("renumber-windows", Scope::Session)),
        en: "Renumber windows",
        ko: "창 번호 다시 매기기",
        help_en: "Closes gaps in window numbers after a window closes.",
        help_ko: "창을 닫으면 빈 번호를 채웁니다.",
    },
    Setting {
        key: "@rmux-history",
        default: "10000",
        kind: Kind::Choices(&[
            choice("2000", "2,000", "2,000"),
            choice("10000", "10,000", "10,000"),
            choice("50000", "50,000", "50,000"),
        ]),
        group: Group::Input,
        sections: &["base"],
        readback: Some(("history-limit", Scope::Session)),
        en: "Scrollback lines",
        ko: "스크롤 기록",
        help_en: "Applies to panes created after the change.",
        help_ko: "변경 후 만든 pane부터 적용됩니다.",
    },
    Setting {
        key: "@rmux-theme",
        default: "dark",
        kind: Kind::Choices(&[
            choice("dark", "Dark", "어둡게"),
            choice("light", "Light", "밝게"),
            choice("terminal", "Terminal colors", "터미널 색"),
            choice("tmux", "tmux default", "tmux 기본"),
        ]),
        group: Group::Look,
        sections: &["colors", "styles"],
        readback: None,
        en: "Theme",
        ko: "테마",
        help_en: "Colors for the bar, borders and menus.",
        help_ko: "상태줄, 경계선, 메뉴의 색입니다.",
    },
    Setting {
        key: "@rmux-lang",
        default: "en",
        kind: Kind::Choices(&[
            choice("en", "English", "English"),
            choice("ko", "한국어", "한국어"),
        ]),
        group: Group::Look,
        sections: &["keys"],
        readback: None,
        en: "Language",
        ko: "언어",
        help_en: "Language of rmux buttons, menus and this screen.",
        help_ko: "rmux 버튼, 메뉴, 이 화면의 언어입니다.",
    },
];

pub(crate) fn setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|setting| setting.key == key)
}
