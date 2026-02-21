#![cfg(target_os = "macos")]
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
};

use core_foundation::{
    base::Boolean,
    runloop::{kCFRunLoopCommonModes, CFRunLoop},
};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};
use tauri_plugin_global_shortcut::ShortcutState;
use tracing::{debug, warn};

pub const NX_DEVICELCTLKEYMASK: u64 = 0x00000001;
pub const NX_DEVICERCTLKEYMASK: u64 = 0x00002000;
pub const NX_DEVICELSHIFTKEYMASK: u64 = 0x00000002;
pub const NX_DEVICERSHIFTKEYMASK: u64 = 0x00000004;
pub const NX_DEVICELCMDKEYMASK: u64 = 0x00000008;
pub const NX_DEVICERCMDKEYMASK: u64 = 0x00000010;
pub const NX_DEVICELALTKEYMASK: u64 = 0x00000020;
pub const NX_DEVICERALTKEYMASK: u64 = 0x00000040;
#[allow(non_upper_case_globals)]
pub const kCGEventFlagMaskSecondaryFn: u64 = 0x00800000;

const KEY_CODE_LEFT_COMMAND: u16 = 0x37;
const KEY_CODE_RIGHT_COMMAND: u16 = 0x36;
const KEY_CODE_LEFT_SHIFT: u16 = 0x38;
const KEY_CODE_RIGHT_SHIFT: u16 = 0x3C;
const KEY_CODE_LEFT_ALT: u16 = 0x3A;
const KEY_CODE_RIGHT_ALT: u16 = 0x3D;
const KEY_CODE_LEFT_CONTROL: u16 = 0x3B;
const KEY_CODE_RIGHT_CONTROL: u16 = 0x3E;
const KEY_CODE_FN: u16 = 0x3F;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> Boolean;
}

pub type HotkeyCallback = Arc<dyn Fn(ShortcutState) + Send + Sync + 'static>;

pub trait HotkeyBackend: Send + Sync {
    fn start(&self) -> Result<(), String>;
    fn stop(&self) -> Result<(), String>;
    fn register_hotkey(&self, shortcut: &str, callback: HotkeyCallback) -> Result<(), String>;
    fn unregister_hotkey(&self, shortcut: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventTapMode {
    #[default]
    Passive,
    Active,
}

impl EventTapMode {
    fn to_tap_options(self) -> CGEventTapOptions {
        match self {
            Self::Passive => CGEventTapOptions::ListenOnly,
            Self::Active => CGEventTapOptions::Default,
        }
    }
}

pub struct MacOSEventTapHotkey {
    inner: Arc<InnerState>,
}

impl Default for MacOSEventTapHotkey {
    fn default() -> Self {
        Self::new(EventTapMode::Passive)
    }
}

impl MacOSEventTapHotkey {
    pub fn new(mode: EventTapMode) -> Self {
        Self {
            inner: Arc::new(InnerState {
                mode,
                hotkeys: Mutex::new(HashMap::new()),
                thread_handle: Mutex::new(None),
            }),
        }
    }

    pub fn has_accessibility_permission() -> bool {
        // SAFETY: AXIsProcessTrusted takes no parameters and returns process trust status.
        unsafe { AXIsProcessTrusted() != 0 }
    }
}

impl HotkeyBackend for MacOSEventTapHotkey {
    fn start(&self) -> Result<(), String> {
        if !Self::has_accessibility_permission() {
            return Err(
                "Accessibility permission is required to start the macOS event tap hotkey backend"
                    .to_string(),
            );
        }

        {
            let thread_handle = self.inner.thread_handle.lock().map_err(|_| lock_error())?;
            if thread_handle.is_some() {
                return Ok(());
            }
        }

        let (startup_tx, startup_rx) = mpsc::channel::<Result<CFRunLoop, String>>();
        let state = Arc::clone(&self.inner);
        let join_handle = thread::Builder::new()
            .name("buzz-hotkey-event-tap".to_string())
            .spawn(move || run_event_tap_thread(state, startup_tx))
            .map_err(|error| format!("Failed to spawn event tap thread: {error}"))?;

        match startup_rx.recv() {
            Ok(Ok(run_loop)) => {
                let mut thread_handle =
                    self.inner.thread_handle.lock().map_err(|_| lock_error())?;
                if thread_handle.is_some() {
                    run_loop.stop();
                    let _ = join_handle.join();
                    return Ok(());
                }

                *thread_handle = Some(TapThreadHandle {
                    run_loop,
                    join_handle,
                });

                debug!("macOS event tap hotkey backend started");
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = join_handle.join();
                Err(error)
            }
            Err(error) => {
                let _ = join_handle.join();
                Err(format!(
                    "Event tap backend startup channel closed unexpectedly: {error}"
                ))
            }
        }
    }

    fn stop(&self) -> Result<(), String> {
        let thread_handle = {
            let mut handle = self.inner.thread_handle.lock().map_err(|_| lock_error())?;
            handle.take()
        };

        if let Some(TapThreadHandle {
            run_loop,
            join_handle,
        }) = thread_handle
        {
            run_loop.stop();
            join_handle
                .join()
                .map_err(|_| "Event tap thread panicked while stopping".to_string())?;
            debug!("macOS event tap hotkey backend stopped");
        }

        if let Ok(mut hotkeys) = self.inner.hotkeys.lock() {
            for hotkey in hotkeys.values_mut() {
                hotkey.pressed = false;
            }
        }

        Ok(())
    }

    fn register_hotkey(&self, shortcut: &str, callback: HotkeyCallback) -> Result<(), String> {
        let parsed = ParsedShortcut::parse(shortcut)?;

        let mut hotkeys = self.inner.hotkeys.lock().map_err(|_| lock_error())?;
        hotkeys.insert(
            parsed.normalized.clone(),
            RegisteredHotkey {
                shortcut: parsed,
                callback,
                pressed: false,
            },
        );

        Ok(())
    }

    fn unregister_hotkey(&self, shortcut: &str) -> Result<(), String> {
        let parsed = ParsedShortcut::parse(shortcut)?;
        let mut hotkeys = self.inner.hotkeys.lock().map_err(|_| lock_error())?;
        hotkeys.remove(parsed.normalized.as_str());
        Ok(())
    }
}

struct InnerState {
    mode: EventTapMode,
    hotkeys: Mutex<HashMap<String, RegisteredHotkey>>,
    thread_handle: Mutex<Option<TapThreadHandle>>,
}

struct TapThreadHandle {
    run_loop: CFRunLoop,
    join_handle: JoinHandle<()>,
}

#[derive(Clone)]
struct RegisteredHotkey {
    shortcut: ParsedShortcut,
    callback: HotkeyCallback,
    pressed: bool,
}

impl RegisteredHotkey {
    fn evaluate(&mut self, snapshot: &KeyEventSnapshot) -> Option<ShortcutState> {
        match snapshot.event_type {
            CGEventType::KeyDown => {
                if snapshot.autorepeat || self.pressed {
                    return None;
                }

                if self.shortcut.key_code == snapshot.key_code
                    && self.shortcut.modifiers.matches(snapshot.modifiers)
                {
                    self.pressed = true;
                    return Some(ShortcutState::Pressed);
                }

                None
            }
            CGEventType::KeyUp => {
                if self.pressed && self.shortcut.key_code == snapshot.key_code {
                    self.pressed = false;
                    return Some(ShortcutState::Released);
                }

                None
            }
            CGEventType::FlagsChanged => {
                if self.shortcut.key_code != snapshot.key_code {
                    return None;
                }

                let mut modifiers_for_match = snapshot.modifiers;
                let key_down = modifier_key_is_pressed(snapshot.key_code, snapshot.modifiers);
                if key_down.is_some() {
                    clear_modifier_for_key(snapshot.key_code, &mut modifiers_for_match);
                }

                let modifiers_match = self.shortcut.modifiers.matches(modifiers_for_match);
                let active_now = key_down.unwrap_or(true) && modifiers_match;
                match (self.pressed, active_now) {
                    (false, true) => {
                        self.pressed = true;
                        Some(ShortcutState::Pressed)
                    }
                    (true, false) => {
                        self.pressed = false;
                        Some(ShortcutState::Released)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ParsedShortcut {
    normalized: String,
    key_code: u16,
    modifiers: ShortcutModifiers,
}

impl ParsedShortcut {
    fn parse(shortcut: &str) -> Result<Self, String> {
        let tokens: Vec<&str> = shortcut
            .split('+')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect();

        if tokens.is_empty() {
            return Err("Hotkey cannot be empty".to_string());
        }

        let (modifier_tokens, key_token) = tokens.split_at(tokens.len() - 1);
        let key_token = key_token
            .first()
            .copied()
            .ok_or_else(|| "Hotkey is missing a key token".to_string())?;

        let modifiers = ShortcutModifiers::from_tokens(modifier_tokens)?;
        let parsed_key = parse_key_token(key_token)
            .ok_or_else(|| format!("Unsupported macOS key token `{key_token}`"))?;

        let mut parts = modifiers.canonical_tokens();
        parts.push(parsed_key.canonical_name);
        let normalized = parts.join("+");

        Ok(Self {
            normalized,
            key_code: parsed_key.key_code,
            modifiers,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct ShortcutModifiers {
    fn_key: bool,
    alt: SideModifierRequirement,
    shift: SideModifierRequirement,
    control: SideModifierRequirement,
    command: SideModifierRequirement,
}

impl ShortcutModifiers {
    fn from_tokens(tokens: &[&str]) -> Result<Self, String> {
        let mut modifiers = Self::default();

        for token in tokens {
            let normalized = token.to_ascii_uppercase();
            match normalized.as_str() {
                "FN" => modifiers.fn_key = true,
                "ALT" | "OPTION" => {
                    modifiers.alt = modifiers.alt.require_any(token)?;
                }
                "LALT" | "LEFTALT" | "LEFTOPTION" => {
                    modifiers.alt = modifiers.alt.require_left(token)?;
                }
                "RALT" | "RIGHTALT" | "RIGHTOPTION" | "ALTGR" => {
                    modifiers.alt = modifiers.alt.require_right(token)?;
                }
                "SHIFT" => {
                    modifiers.shift = modifiers.shift.require_any(token)?;
                }
                "LSHIFT" | "LEFTSHIFT" => {
                    modifiers.shift = modifiers.shift.require_left(token)?;
                }
                "RSHIFT" | "RIGHTSHIFT" => {
                    modifiers.shift = modifiers.shift.require_right(token)?;
                }
                "CTRL" | "CONTROL" => {
                    modifiers.control = modifiers.control.require_any(token)?;
                }
                "LCTRL" | "LEFTCTRL" | "LEFTCONTROL" => {
                    modifiers.control = modifiers.control.require_left(token)?;
                }
                "RCTRL" | "RIGHTCTRL" | "RIGHTCONTROL" => {
                    modifiers.control = modifiers.control.require_right(token)?;
                }
                "CMD" | "COMMAND" | "META" | "SUPER" => {
                    modifiers.command = modifiers.command.require_any(token)?;
                }
                "LCMD" | "LEFTCMD" | "LEFTCOMMAND" | "LMETA" | "LEFTMETA" | "LSUPER"
                | "LEFTSUPER" => {
                    modifiers.command = modifiers.command.require_left(token)?;
                }
                "RCMD" | "RIGHTCMD" | "RIGHTCOMMAND" | "RMETA" | "RIGHTMETA" | "RSUPER"
                | "RIGHTSUPER" => {
                    modifiers.command = modifiers.command.require_right(token)?;
                }
                _ => return Err(format!("Unsupported modifier token `{token}`")),
            }
        }

        Ok(modifiers)
    }

    fn matches(self, state: ModifierState) -> bool {
        self.fn_key == state.fn_key
            && self.alt.matches(state.left_alt, state.right_alt)
            && self.shift.matches(state.left_shift, state.right_shift)
            && self
                .control
                .matches(state.left_control, state.right_control)
            && self
                .command
                .matches(state.left_command, state.right_command)
    }

    fn canonical_tokens(self) -> Vec<String> {
        let mut tokens = Vec::new();

        push_side_modifier_tokens(&mut tokens, self.control, "Ctrl", "LCtrl", "RCtrl");
        push_side_modifier_tokens(&mut tokens, self.alt, "Alt", "LAlt", "RAlt");
        push_side_modifier_tokens(&mut tokens, self.shift, "Shift", "LShift", "RShift");
        push_side_modifier_tokens(&mut tokens, self.command, "Cmd", "LCmd", "RCmd");

        if self.fn_key {
            tokens.push("Fn".to_string());
        }

        tokens
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
enum SideModifierRequirement {
    #[default]
    None,
    Any,
    Left,
    Right,
    Both,
}

impl SideModifierRequirement {
    fn require_any(self, token: &str) -> Result<Self, String> {
        match self {
            Self::None | Self::Any => Ok(Self::Any),
            Self::Left | Self::Right | Self::Both => Err(format!(
                "Modifier token `{token}` conflicts with side-specific modifier tokens"
            )),
        }
    }

    fn require_left(self, token: &str) -> Result<Self, String> {
        match self {
            Self::None | Self::Left => Ok(Self::Left),
            Self::Right | Self::Both => Ok(Self::Both),
            Self::Any => Err(format!(
                "Modifier token `{token}` conflicts with side-agnostic modifier token"
            )),
        }
    }

    fn require_right(self, token: &str) -> Result<Self, String> {
        match self {
            Self::None | Self::Right => Ok(Self::Right),
            Self::Left | Self::Both => Ok(Self::Both),
            Self::Any => Err(format!(
                "Modifier token `{token}` conflicts with side-agnostic modifier token"
            )),
        }
    }

    fn matches(self, left: bool, right: bool) -> bool {
        match self {
            Self::None => !left && !right,
            Self::Any => left || right,
            Self::Left => left && !right,
            Self::Right => !left && right,
            Self::Both => left && right,
        }
    }
}

fn push_side_modifier_tokens(
    tokens: &mut Vec<String>,
    requirement: SideModifierRequirement,
    any: &str,
    left: &str,
    right: &str,
) {
    match requirement {
        SideModifierRequirement::None => {}
        SideModifierRequirement::Any => tokens.push(any.to_string()),
        SideModifierRequirement::Left => tokens.push(left.to_string()),
        SideModifierRequirement::Right => tokens.push(right.to_string()),
        SideModifierRequirement::Both => {
            tokens.push(left.to_string());
            tokens.push(right.to_string());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierState {
    fn_key: bool,
    left_alt: bool,
    right_alt: bool,
    left_shift: bool,
    right_shift: bool,
    left_control: bool,
    right_control: bool,
    left_command: bool,
    right_command: bool,
}

impl ModifierState {
    fn from_raw_flags(raw_flags: u64) -> Self {
        Self {
            fn_key: raw_flags & kCGEventFlagMaskSecondaryFn != 0,
            left_alt: raw_flags & NX_DEVICELALTKEYMASK != 0,
            right_alt: raw_flags & NX_DEVICERALTKEYMASK != 0,
            left_shift: raw_flags & NX_DEVICELSHIFTKEYMASK != 0,
            right_shift: raw_flags & NX_DEVICERSHIFTKEYMASK != 0,
            left_control: raw_flags & NX_DEVICELCTLKEYMASK != 0,
            right_control: raw_flags & NX_DEVICERCTLKEYMASK != 0,
            left_command: raw_flags & NX_DEVICELCMDKEYMASK != 0,
            right_command: raw_flags & NX_DEVICERCMDKEYMASK != 0,
        }
    }
}

fn modifier_key_is_pressed(key_code: u16, modifiers: ModifierState) -> Option<bool> {
    match key_code {
        KEY_CODE_LEFT_ALT => Some(modifiers.left_alt),
        KEY_CODE_RIGHT_ALT => Some(modifiers.right_alt),
        KEY_CODE_LEFT_SHIFT => Some(modifiers.left_shift),
        KEY_CODE_RIGHT_SHIFT => Some(modifiers.right_shift),
        KEY_CODE_LEFT_CONTROL => Some(modifiers.left_control),
        KEY_CODE_RIGHT_CONTROL => Some(modifiers.right_control),
        KEY_CODE_LEFT_COMMAND => Some(modifiers.left_command),
        KEY_CODE_RIGHT_COMMAND => Some(modifiers.right_command),
        KEY_CODE_FN => Some(modifiers.fn_key),
        _ => None,
    }
}

fn clear_modifier_for_key(key_code: u16, modifiers: &mut ModifierState) {
    match key_code {
        KEY_CODE_LEFT_ALT => modifiers.left_alt = false,
        KEY_CODE_RIGHT_ALT => modifiers.right_alt = false,
        KEY_CODE_LEFT_SHIFT => modifiers.left_shift = false,
        KEY_CODE_RIGHT_SHIFT => modifiers.right_shift = false,
        KEY_CODE_LEFT_CONTROL => modifiers.left_control = false,
        KEY_CODE_RIGHT_CONTROL => modifiers.right_control = false,
        KEY_CODE_LEFT_COMMAND => modifiers.left_command = false,
        KEY_CODE_RIGHT_COMMAND => modifiers.right_command = false,
        KEY_CODE_FN => modifiers.fn_key = false,
        _ => {}
    }
}

#[derive(Debug, Clone, Copy)]
struct KeyEventSnapshot {
    event_type: CGEventType,
    key_code: u16,
    modifiers: ModifierState,
    autorepeat: bool,
}

impl KeyEventSnapshot {
    fn from_event(event_type: CGEventType, event: &CGEvent) -> Option<Self> {
        let key_code =
            u16::try_from(event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE))
                .ok()?;
        let flags = event.get_flags().bits();
        let autorepeat = matches!(event_type, CGEventType::KeyDown)
            && event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;

        Some(Self {
            event_type,
            key_code,
            modifiers: ModifierState::from_raw_flags(flags),
            autorepeat,
        })
    }
}

#[derive(Debug, Clone)]
struct ParsedKey {
    key_code: u16,
    canonical_name: String,
}

fn parse_key_token(token: &str) -> Option<ParsedKey> {
    let normalized = token.trim().to_ascii_uppercase();

    if normalized.len() == 1 {
        return parse_single_character_key(normalized.chars().next()?).map(|key_code| ParsedKey {
            key_code,
            canonical_name: normalized,
        });
    }

    if let Some(number_str) = normalized.strip_prefix('F') {
        if let Ok(number) = number_str.parse::<u8>() {
            if let Some(key_code) = function_key_code(number) {
                return Some(ParsedKey {
                    key_code,
                    canonical_name: format!("F{number}"),
                });
            }
        }
    }

    let (key_code, canonical_name) = match normalized.as_str() {
        "SPACE" => (0x31, "Space"),
        "TAB" => (0x30, "Tab"),
        "ENTER" | "RETURN" => (0x24, "Enter"),
        "ESC" | "ESCAPE" => (0x35, "Escape"),
        "BACKSPACE" => (0x33, "Backspace"),
        "DELETE" | "FORWARDDELETE" => (0x75, "Delete"),
        "HOME" => (0x73, "Home"),
        "END" => (0x77, "End"),
        "PAGEUP" => (0x74, "PageUp"),
        "PAGEDOWN" => (0x79, "PageDown"),
        "LEFT" | "LEFTARROW" => (0x7B, "Left"),
        "RIGHT" | "RIGHTARROW" => (0x7C, "Right"),
        "UP" | "UPARROW" => (0x7E, "Up"),
        "DOWN" | "DOWNARROW" => (0x7D, "Down"),
        "MINUS" => (0x1B, "Minus"),
        "EQUAL" | "EQUALS" => (0x18, "Equal"),
        "LEFTBRACKET" => (0x21, "LeftBracket"),
        "RIGHTBRACKET" => (0x1E, "RightBracket"),
        "SEMICOLON" => (0x29, "Semicolon"),
        "QUOTE" | "APOSTROPHE" => (0x27, "Quote"),
        "BACKSLASH" => (0x2A, "Backslash"),
        "COMMA" => (0x2B, "Comma"),
        "PERIOD" | "DOT" => (0x2F, "Period"),
        "SLASH" => (0x2C, "Slash"),
        "GRAVE" | "BACKTICK" => (0x32, "Grave"),
        "FN" => (0x3F, "Fn"),
        "LALT" | "LEFTALT" | "LEFTOPTION" => (0x3A, "LAlt"),
        "RALT" | "RIGHTALT" | "RIGHTOPTION" | "ALTGR" => (0x3D, "RAlt"),
        "LSHIFT" | "LEFTSHIFT" => (0x38, "LShift"),
        "RSHIFT" | "RIGHTSHIFT" => (0x3C, "RShift"),
        "LCTRL" | "LEFTCTRL" | "LEFTCONTROL" => (0x3B, "LCtrl"),
        "RCTRL" | "RIGHTCTRL" | "RIGHTCONTROL" => (0x3E, "RCtrl"),
        "LCMD" | "LEFTCMD" | "LEFTCOMMAND" => (0x37, "LCmd"),
        "RCMD" | "RIGHTCMD" | "RIGHTCOMMAND" => (0x36, "RCmd"),
        _ => return None,
    };

    Some(ParsedKey {
        key_code,
        canonical_name: canonical_name.to_string(),
    })
}

fn parse_single_character_key(ch: char) -> Option<u16> {
    match ch {
        'A' => Some(0x00),
        'B' => Some(0x0B),
        'C' => Some(0x08),
        'D' => Some(0x02),
        'E' => Some(0x0E),
        'F' => Some(0x03),
        'G' => Some(0x05),
        'H' => Some(0x04),
        'I' => Some(0x22),
        'J' => Some(0x26),
        'K' => Some(0x28),
        'L' => Some(0x25),
        'M' => Some(0x2E),
        'N' => Some(0x2D),
        'O' => Some(0x1F),
        'P' => Some(0x23),
        'Q' => Some(0x0C),
        'R' => Some(0x0F),
        'S' => Some(0x01),
        'T' => Some(0x11),
        'U' => Some(0x20),
        'V' => Some(0x09),
        'W' => Some(0x0D),
        'X' => Some(0x07),
        'Y' => Some(0x10),
        'Z' => Some(0x06),
        '0' => Some(0x1D),
        '1' => Some(0x12),
        '2' => Some(0x13),
        '3' => Some(0x14),
        '4' => Some(0x15),
        '5' => Some(0x17),
        '6' => Some(0x16),
        '7' => Some(0x1A),
        '8' => Some(0x1C),
        '9' => Some(0x19),
        '-' => Some(0x1B),
        '=' => Some(0x18),
        '[' => Some(0x21),
        ']' => Some(0x1E),
        ';' => Some(0x29),
        '\'' => Some(0x27),
        '\\' => Some(0x2A),
        ',' => Some(0x2B),
        '.' => Some(0x2F),
        '/' => Some(0x2C),
        '`' => Some(0x32),
        _ => None,
    }
}

fn function_key_code(number: u8) -> Option<u16> {
    match number {
        1 => Some(0x7A),
        2 => Some(0x78),
        3 => Some(0x63),
        4 => Some(0x76),
        5 => Some(0x60),
        6 => Some(0x61),
        7 => Some(0x62),
        8 => Some(0x64),
        9 => Some(0x65),
        10 => Some(0x6D),
        11 => Some(0x67),
        12 => Some(0x6F),
        13 => Some(0x69),
        14 => Some(0x6B),
        15 => Some(0x71),
        16 => Some(0x6A),
        17 => Some(0x40),
        18 => Some(0x4F),
        19 => Some(0x50),
        20 => Some(0x5A),
        _ => None,
    }
}

fn run_event_tap_thread(
    state: Arc<InnerState>,
    startup_tx: mpsc::Sender<Result<CFRunLoop, String>>,
) {
    let run_loop = CFRunLoop::get_current();

    let tap = match CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        state.mode.to_tap_options(),
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ],
        move |_proxy, event_type, event| {
            match event_type {
                CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                    state.dispatch_event(event_type, event)
                }
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    warn!(
                        ?event_type,
                        "macOS event tap was disabled by the system; hotkeys may stop firing until restarted"
                    );
                }
                _ => {}
            }
            None
        },
    ) {
        Ok(tap) => tap,
        Err(_) => {
            let _ = startup_tx.send(Err("Failed to create CGEventTap".to_string()));
            return;
        }
    };

    let source = match tap.mach_port.create_runloop_source(0) {
        Ok(source) => source,
        Err(_) => {
            let _ = startup_tx.send(Err("Failed to create event tap runloop source".to_string()));
            return;
        }
    };

    // SAFETY: `kCFRunLoopCommonModes` is a valid CoreFoundation runloop mode.
    unsafe {
        run_loop.add_source(&source, kCFRunLoopCommonModes);
    }
    tap.enable();

    if startup_tx.send(Ok(run_loop.clone())).is_err() {
        return;
    }

    CFRunLoop::run_current();

    // SAFETY: `kCFRunLoopCommonModes` is the same mode used for add_source above.
    unsafe {
        run_loop.remove_source(&source, kCFRunLoopCommonModes);
    }
}

impl InnerState {
    fn dispatch_event(&self, event_type: CGEventType, event: &CGEvent) {
        let Some(snapshot) = KeyEventSnapshot::from_event(event_type, event) else {
            return;
        };

        let callbacks = {
            let mut hotkeys = match self.hotkeys.lock() {
                Ok(hotkeys) => hotkeys,
                Err(_) => return,
            };

            let mut callbacks = Vec::<(HotkeyCallback, ShortcutState)>::new();
            for hotkey in hotkeys.values_mut() {
                if let Some(state) = hotkey.evaluate(&snapshot) {
                    callbacks.push((Arc::clone(&hotkey.callback), state));
                }
            }

            callbacks
        };

        for (callback, state) in callbacks {
            callback(state);
        }
    }
}

fn lock_error() -> String {
    "macOS event tap hotkey state lock was poisoned".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_flags_with_fn_and_side_specific_masks() {
        let flags = kCGEventFlagMaskSecondaryFn
            | NX_DEVICELALTKEYMASK
            | NX_DEVICERSHIFTKEYMASK
            | NX_DEVICELCMDKEYMASK
            | NX_DEVICERCTLKEYMASK;

        let parsed = ModifierState::from_raw_flags(flags);

        assert!(parsed.fn_key);
        assert!(parsed.left_alt);
        assert!(!parsed.right_alt);
        assert!(!parsed.left_shift);
        assert!(parsed.right_shift);
        assert!(!parsed.left_control);
        assert!(parsed.right_control);
        assert!(parsed.left_command);
        assert!(!parsed.right_command);
    }

    #[test]
    fn parses_fn_lalt_ralt_shortcuts() {
        let fn_alt = ParsedShortcut::parse("Fn+LAlt+Space").expect("shortcut should parse");
        assert_eq!(fn_alt.key_code, 0x31);
        assert!(fn_alt.modifiers.fn_key);
        assert_eq!(fn_alt.modifiers.alt, SideModifierRequirement::Left);

        let right_alt = ParsedShortcut::parse("RAlt+Space").expect("shortcut should parse");
        assert_eq!(right_alt.modifiers.alt, SideModifierRequirement::Right);
    }

    #[test]
    fn rejects_conflicting_side_and_non_side_modifier_tokens() {
        let error =
            ParsedShortcut::parse("Alt+LAlt+Space").expect_err("conflicting tokens should fail");
        assert!(error.contains("conflicts"));
    }

    #[test]
    fn side_aware_matching_distinguishes_left_and_right_alt() {
        let parsed = ParsedShortcut::parse("RAlt+Space").expect("shortcut should parse");

        let right_alt_match = ModifierState::from_raw_flags(NX_DEVICERALTKEYMASK);
        let left_alt_match = ModifierState::from_raw_flags(NX_DEVICELALTKEYMASK);

        assert!(parsed.modifiers.matches(right_alt_match));
        assert!(!parsed.modifiers.matches(left_alt_match));
    }

    #[test]
    fn hotkey_matching_emits_pressed_and_released_events() {
        let parsed = ParsedShortcut::parse("Fn+RAlt+Space").expect("shortcut should parse");
        let callback: HotkeyCallback = Arc::new(|_| {});
        let mut registered = RegisteredHotkey {
            shortcut: parsed,
            callback,
            pressed: false,
        };

        let key_down = KeyEventSnapshot {
            event_type: CGEventType::KeyDown,
            key_code: 0x31,
            modifiers: ModifierState::from_raw_flags(
                kCGEventFlagMaskSecondaryFn | NX_DEVICERALTKEYMASK,
            ),
            autorepeat: false,
        };
        assert_eq!(registered.evaluate(&key_down), Some(ShortcutState::Pressed));

        let autorepeat = KeyEventSnapshot {
            autorepeat: true,
            ..key_down
        };
        assert_eq!(registered.evaluate(&autorepeat), None);

        let key_up = KeyEventSnapshot {
            event_type: CGEventType::KeyUp,
            ..key_down
        };
        assert_eq!(registered.evaluate(&key_up), Some(ShortcutState::Released));
    }

    #[test]
    fn flags_changed_can_drive_modifier_key_shortcuts() {
        let parsed = ParsedShortcut::parse("Fn+RAlt").expect("shortcut should parse");
        let callback: HotkeyCallback = Arc::new(|_| {});
        let mut registered = RegisteredHotkey {
            shortcut: parsed,
            callback,
            pressed: false,
        };

        let pressed = KeyEventSnapshot {
            event_type: CGEventType::FlagsChanged,
            key_code: 0x3D,
            modifiers: ModifierState::from_raw_flags(
                kCGEventFlagMaskSecondaryFn | NX_DEVICERALTKEYMASK,
            ),
            autorepeat: false,
        };
        assert_eq!(registered.evaluate(&pressed), Some(ShortcutState::Pressed));

        let released = KeyEventSnapshot {
            modifiers: ModifierState::from_raw_flags(kCGEventFlagMaskSecondaryFn),
            ..pressed
        };
        assert_eq!(
            registered.evaluate(&released),
            Some(ShortcutState::Released)
        );
    }

    #[test]
    #[ignore = "requires macOS Accessibility permission and manual keyboard interaction"]
    fn integration_smoke_start_and_stop_event_tap_backend() {
        if !MacOSEventTapHotkey::has_accessibility_permission() {
            return;
        }

        let backend = MacOSEventTapHotkey::default();
        backend.start().expect("event tap should start");
        backend.stop().expect("event tap should stop");
    }
}
