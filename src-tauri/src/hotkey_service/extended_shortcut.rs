#![allow(dead_code)]

use std::{collections::BTreeSet, fmt, str::FromStr};

use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtendedModifier {
    LAlt,
    RAlt,
    Alt,
    LShift,
    RShift,
    Shift,
    LCtrl,
    RCtrl,
    Ctrl,
    LMeta,
    RMeta,
    Meta,
    Fn,
}

impl ExtendedModifier {
    fn parse(token: &str) -> Option<Self> {
        let normalized = normalize_modifier_token(token);

        let modifier = match normalized.as_str() {
            "LALT" | "LEFTALT" | "ALTLEFT" | "LOPTION" | "LEFTOPTION" | "OPTIONLEFT" => Self::LAlt,
            "RALT" | "RIGHTALT" | "ALTRIGHT" | "ROPTION" | "RIGHTOPTION" | "OPTIONRIGHT" => {
                Self::RAlt
            }
            "ALT" | "OPTION" => Self::Alt,
            "LSHIFT" | "LEFTSHIFT" | "SHIFTLEFT" => Self::LShift,
            "RSHIFT" | "RIGHTSHIFT" | "SHIFTRIGHT" => Self::RShift,
            "SHIFT" => Self::Shift,
            "LCTRL" | "LCONTROL" | "LEFTCTRL" | "LEFTCONTROL" | "CTRLLEFT" | "CONTROLLEFT" => {
                Self::LCtrl
            }
            "RCTRL" | "RCONTROL" | "RIGHTCTRL" | "RIGHTCONTROL" | "CTRLRIGHT" | "CONTROLRIGHT" => {
                Self::RCtrl
            }
            "CTRL" | "CONTROL" => Self::Ctrl,
            "LMETA" | "LEFTMETA" | "METALEFT" | "LCMD" | "LEFTCMD" | "CMDLEFT" | "LCOMMAND"
            | "LEFTCOMMAND" | "COMMANDLEFT" | "LSUPER" | "LEFTSUPER" | "SUPERLEFT" | "LOS"
            | "LEFTOS" | "OSLEFT" => Self::LMeta,
            "RMETA" | "RIGHTMETA" | "METARIGHT" | "RCMD" | "RIGHTCMD" | "CMDRIGHT" | "RCOMMAND"
            | "RIGHTCOMMAND" | "COMMANDRIGHT" | "RSUPER" | "RIGHTSUPER" | "SUPERRIGHT" | "ROS"
            | "RIGHTOS" | "OSRIGHT" => Self::RMeta,
            "META" | "CMD" | "COMMAND" | "SUPER" | "OS" => Self::Meta,
            "FN" | "FUNCTION" => Self::Fn,
            "COMMANDORCONTROL" | "COMMANDORCTRL" | "CMDORCTRL" | "CMDORCONTROL" => {
                #[cfg(target_os = "macos")]
                {
                    Self::Meta
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Self::Ctrl
                }
            }
            _ => return None,
        };

        Some(modifier)
    }

    fn as_token(self) -> &'static str {
        match self {
            Self::LAlt => "LAlt",
            Self::RAlt => "RAlt",
            Self::Alt => "Alt",
            Self::LShift => "LShift",
            Self::RShift => "RShift",
            Self::Shift => "Shift",
            Self::LCtrl => "LCtrl",
            Self::RCtrl => "RCtrl",
            Self::Ctrl => "Ctrl",
            Self::LMeta => "LMeta",
            Self::RMeta => "RMeta",
            Self::Meta => "Cmd",
            Self::Fn => "Fn",
        }
    }
}

const MODIFIER_DISPLAY_ORDER: [ExtendedModifier; 13] = [
    ExtendedModifier::LCtrl,
    ExtendedModifier::RCtrl,
    ExtendedModifier::Ctrl,
    ExtendedModifier::LAlt,
    ExtendedModifier::RAlt,
    ExtendedModifier::Alt,
    ExtendedModifier::LShift,
    ExtendedModifier::RShift,
    ExtendedModifier::Shift,
    ExtendedModifier::LMeta,
    ExtendedModifier::RMeta,
    ExtendedModifier::Meta,
    ExtendedModifier::Fn,
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PressedModifiers {
    pub l_alt: bool,
    pub r_alt: bool,
    pub l_shift: bool,
    pub r_shift: bool,
    pub l_ctrl: bool,
    pub r_ctrl: bool,
    pub l_meta: bool,
    pub r_meta: bool,
    pub fn_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedShortcut {
    modifiers: BTreeSet<ExtendedModifier>,
    key: Code,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtendedShortcutParseError {
    EmptyShortcut,
    EmptyToken,
    MissingKey,
    InvalidKeyToken(String),
}

impl fmt::Display for ExtendedShortcutParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyShortcut => f.write_str("Shortcut cannot be empty"),
            Self::EmptyToken => f.write_str("Shortcut contains an empty token"),
            Self::MissingKey => f.write_str("Shortcut must include a non-modifier key"),
            Self::InvalidKeyToken(token) => {
                write!(f, "Unsupported key token `{token}`")
            }
        }
    }
}

impl std::error::Error for ExtendedShortcutParseError {}

impl ExtendedShortcut {
    pub fn parse(shortcut: &str) -> Result<Self, ExtendedShortcutParseError> {
        shortcut.parse()
    }

    pub fn key(&self) -> Code {
        self.key
    }

    pub fn modifiers(&self) -> impl Iterator<Item = ExtendedModifier> + '_ {
        MODIFIER_DISPLAY_ORDER
            .iter()
            .copied()
            .filter(|modifier| self.modifiers.contains(modifier))
    }

    pub fn has_side_specific_modifiers(&self) -> bool {
        self.modifiers.contains(&ExtendedModifier::LAlt)
            || self.modifiers.contains(&ExtendedModifier::RAlt)
            || self.modifiers.contains(&ExtendedModifier::LShift)
            || self.modifiers.contains(&ExtendedModifier::RShift)
            || self.modifiers.contains(&ExtendedModifier::LCtrl)
            || self.modifiers.contains(&ExtendedModifier::RCtrl)
            || self.modifiers.contains(&ExtendedModifier::LMeta)
            || self.modifiers.contains(&ExtendedModifier::RMeta)
    }

    pub fn has_fn_modifier(&self) -> bool {
        self.modifiers.contains(&ExtendedModifier::Fn)
    }

    pub fn matches(&self, pressed_modifiers: &PressedModifiers, pressed_key: Code) -> bool {
        if self.key != pressed_key {
            return false;
        }

        if !matches_family(
            self.requirement_for(
                ExtendedModifier::Alt,
                ExtendedModifier::LAlt,
                ExtendedModifier::RAlt,
            ),
            pressed_modifiers.l_alt,
            pressed_modifiers.r_alt,
        ) {
            return false;
        }

        if !matches_family(
            self.requirement_for(
                ExtendedModifier::Shift,
                ExtendedModifier::LShift,
                ExtendedModifier::RShift,
            ),
            pressed_modifiers.l_shift,
            pressed_modifiers.r_shift,
        ) {
            return false;
        }

        if !matches_family(
            self.requirement_for(
                ExtendedModifier::Ctrl,
                ExtendedModifier::LCtrl,
                ExtendedModifier::RCtrl,
            ),
            pressed_modifiers.l_ctrl,
            pressed_modifiers.r_ctrl,
        ) {
            return false;
        }

        if !matches_family(
            self.requirement_for(
                ExtendedModifier::Meta,
                ExtendedModifier::LMeta,
                ExtendedModifier::RMeta,
            ),
            pressed_modifiers.l_meta,
            pressed_modifiers.r_meta,
        ) {
            return false;
        }

        self.has_fn_modifier() == pressed_modifiers.fn_key
    }

    pub fn to_global_shortcut(&self) -> Shortcut {
        let mut modifiers = Modifiers::empty();
        if self.has_any_ctrl() {
            modifiers |= Modifiers::CONTROL;
        }
        if self.has_any_alt() {
            modifiers |= Modifiers::ALT;
        }
        if self.has_any_shift() {
            modifiers |= Modifiers::SHIFT;
        }
        if self.has_any_meta() {
            modifiers |= Modifiers::SUPER;
        }

        Shortcut::new(Some(modifiers), self.key)
    }

    pub fn to_global_shortcut_string(&self) -> String {
        let mut tokens = Vec::new();
        if self.has_any_ctrl() {
            tokens.push("Ctrl".to_string());
        }
        if self.has_any_alt() {
            tokens.push("Alt".to_string());
        }
        if self.has_any_shift() {
            tokens.push("Shift".to_string());
        }
        if self.has_any_meta() {
            tokens.push("Cmd".to_string());
        }

        tokens.push(format_key_token(self.key));
        tokens.join("+")
    }

    fn has_any_alt(&self) -> bool {
        self.modifiers.contains(&ExtendedModifier::Alt)
            || self.modifiers.contains(&ExtendedModifier::LAlt)
            || self.modifiers.contains(&ExtendedModifier::RAlt)
    }

    fn has_any_shift(&self) -> bool {
        self.modifiers.contains(&ExtendedModifier::Shift)
            || self.modifiers.contains(&ExtendedModifier::LShift)
            || self.modifiers.contains(&ExtendedModifier::RShift)
    }

    fn has_any_ctrl(&self) -> bool {
        self.modifiers.contains(&ExtendedModifier::Ctrl)
            || self.modifiers.contains(&ExtendedModifier::LCtrl)
            || self.modifiers.contains(&ExtendedModifier::RCtrl)
    }

    fn has_any_meta(&self) -> bool {
        self.modifiers.contains(&ExtendedModifier::Meta)
            || self.modifiers.contains(&ExtendedModifier::LMeta)
            || self.modifiers.contains(&ExtendedModifier::RMeta)
    }

    fn requirement_for(
        &self,
        generic: ExtendedModifier,
        left: ExtendedModifier,
        right: ExtendedModifier,
    ) -> FamilyRequirement {
        let has_generic = self.modifiers.contains(&generic);
        let has_left = self.modifiers.contains(&left);
        let has_right = self.modifiers.contains(&right);

        if has_left && has_right {
            FamilyRequirement::Both
        } else if has_left {
            FamilyRequirement::Left
        } else if has_right {
            FamilyRequirement::Right
        } else if has_generic {
            FamilyRequirement::Generic
        } else {
            FamilyRequirement::None
        }
    }
}

impl fmt::Display for ExtendedShortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut tokens = self
            .modifiers()
            .map(ExtendedModifier::as_token)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        tokens.push(format_key_token(self.key));
        f.write_str(&tokens.join("+"))
    }
}

impl FromStr for ExtendedShortcut {
    type Err = ExtendedShortcutParseError;

    fn from_str(shortcut: &str) -> Result<Self, Self::Err> {
        if shortcut.trim().is_empty() {
            return Err(ExtendedShortcutParseError::EmptyShortcut);
        }

        let mut modifiers = BTreeSet::new();
        let mut key = None;

        for raw_token in shortcut.split('+') {
            let token = raw_token.trim();
            if token.is_empty() {
                return Err(ExtendedShortcutParseError::EmptyToken);
            }

            if let Some(modifier) = ExtendedModifier::parse(token) {
                modifiers.insert(modifier);
                continue;
            }

            key = Some(parse_key_token(token)?);
        }

        normalize_modifiers(&mut modifiers);
        let key = key.ok_or(ExtendedShortcutParseError::MissingKey)?;

        Ok(Self { modifiers, key })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FamilyRequirement {
    None,
    Generic,
    Left,
    Right,
    Both,
}

fn matches_family(requirement: FamilyRequirement, left_pressed: bool, right_pressed: bool) -> bool {
    match requirement {
        FamilyRequirement::None => !left_pressed && !right_pressed,
        FamilyRequirement::Generic => left_pressed || right_pressed,
        FamilyRequirement::Left => left_pressed && !right_pressed,
        FamilyRequirement::Right => !left_pressed && right_pressed,
        FamilyRequirement::Both => left_pressed && right_pressed,
    }
}

fn normalize_modifiers(modifiers: &mut BTreeSet<ExtendedModifier>) {
    drop_redundant_generic_modifier(
        modifiers,
        ExtendedModifier::Alt,
        ExtendedModifier::LAlt,
        ExtendedModifier::RAlt,
    );
    drop_redundant_generic_modifier(
        modifiers,
        ExtendedModifier::Shift,
        ExtendedModifier::LShift,
        ExtendedModifier::RShift,
    );
    drop_redundant_generic_modifier(
        modifiers,
        ExtendedModifier::Ctrl,
        ExtendedModifier::LCtrl,
        ExtendedModifier::RCtrl,
    );
    drop_redundant_generic_modifier(
        modifiers,
        ExtendedModifier::Meta,
        ExtendedModifier::LMeta,
        ExtendedModifier::RMeta,
    );
}

fn drop_redundant_generic_modifier(
    modifiers: &mut BTreeSet<ExtendedModifier>,
    generic: ExtendedModifier,
    left: ExtendedModifier,
    right: ExtendedModifier,
) {
    if modifiers.contains(&left) || modifiers.contains(&right) {
        modifiers.remove(&generic);
    }
}

fn normalize_modifier_token(token: &str) -> String {
    token
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

fn parse_key_token(token: &str) -> Result<Code, ExtendedShortcutParseError> {
    token
        .parse::<Shortcut>()
        .map(|shortcut| shortcut.key)
        .map_err(|_| ExtendedShortcutParseError::InvalidKeyToken(token.to_string()))
}

fn format_key_token(key: Code) -> String {
    let key_name = key.to_string();

    if let Some(letter) = key_name.strip_prefix("Key") {
        if letter.len() == 1 {
            return letter.to_string();
        }
    }

    if let Some(digit) = key_name.strip_prefix("Digit") {
        if digit.len() == 1 {
            return digit.to_string();
        }
    }

    match key_name.as_str() {
        "Backquote" => "`".to_string(),
        "Backslash" => "\\".to_string(),
        "BracketLeft" => "[".to_string(),
        "BracketRight" => "]".to_string(),
        "Comma" => ",".to_string(),
        "Equal" => "=".to_string(),
        "Minus" => "-".to_string(),
        "Period" => ".".to_string(),
        "Quote" => "'".to_string(),
        "Semicolon" => ";".to_string(),
        "Slash" => "/".to_string(),
        _ => key_name,
    }
}

#[cfg(test)]
mod tests {
    use tauri_plugin_global_shortcut::Code;

    use super::{ExtendedShortcut, ExtendedShortcutParseError, PressedModifiers};

    #[test]
    fn parses_generic_shortcuts_for_backward_compatibility() {
        let shortcut: ExtendedShortcut = "Alt+Space".parse().expect("shortcut should parse");

        assert_eq!(shortcut.to_string(), "Alt+Space");
        assert_eq!(shortcut.to_global_shortcut_string(), "Alt+Space");
        assert!(shortcut.matches(
            &PressedModifiers {
                l_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
        assert!(shortcut.matches(
            &PressedModifiers {
                r_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
    }

    #[test]
    fn parses_side_specific_shortcuts_case_insensitively() {
        let shortcut: ExtendedShortcut = "ralt+space".parse().expect("shortcut should parse");

        assert_eq!(shortcut.to_string(), "RAlt+Space");
        assert!(shortcut.matches(
            &PressedModifiers {
                r_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
        assert!(!shortcut.matches(
            &PressedModifiers {
                l_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
    }

    #[test]
    fn parses_fn_shortcuts_and_matches_fn_state() {
        let shortcut: ExtendedShortcut = "fn+f5".parse().expect("shortcut should parse");

        assert_eq!(shortcut.to_string(), "Fn+F5");
        assert!(shortcut.matches(
            &PressedModifiers {
                fn_key: true,
                ..Default::default()
            },
            Code::F5
        ));
        assert!(!shortcut.matches(&PressedModifiers::default(), Code::F5));
    }

    #[test]
    fn parser_uses_last_non_modifier_token_as_key() {
        let shortcut: ExtendedShortcut = "A+Shift+S".parse().expect("shortcut should parse");

        assert_eq!(shortcut.to_string(), "Shift+S");
    }

    #[test]
    fn parser_rejects_modifier_only_shortcuts() {
        assert_eq!(
            "Alt+Shift".parse::<ExtendedShortcut>(),
            Err(ExtendedShortcutParseError::MissingKey)
        );
    }

    #[test]
    fn conversion_to_global_shortcut_drops_side_information_and_fn() {
        let shortcut: ExtendedShortcut = "Fn+RAlt+Space".parse().expect("shortcut should parse");

        assert_eq!(shortcut.to_global_shortcut_string(), "Alt+Space");

        let global_shortcut = shortcut.to_global_shortcut();
        assert!(global_shortcut.matches(tauri_plugin_global_shortcut::Modifiers::ALT, Code::Space));
    }

    #[test]
    fn side_specific_matching_requires_exact_side() {
        let shortcut: ExtendedShortcut = "LAlt+Space".parse().expect("shortcut should parse");

        assert!(shortcut.matches(
            &PressedModifiers {
                l_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
        assert!(!shortcut.matches(
            &PressedModifiers {
                r_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
        assert!(!shortcut.matches(
            &PressedModifiers {
                l_alt: true,
                r_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
    }

    #[test]
    fn generic_matching_allows_either_or_both_sides() {
        let shortcut: ExtendedShortcut = "Alt+Space".parse().expect("shortcut should parse");

        assert!(shortcut.matches(
            &PressedModifiers {
                l_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
        assert!(shortcut.matches(
            &PressedModifiers {
                r_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
        assert!(shortcut.matches(
            &PressedModifiers {
                l_alt: true,
                r_alt: true,
                ..Default::default()
            },
            Code::Space
        ));
    }
}
