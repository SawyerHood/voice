use std::{
    ffi::c_void,
    io::Write,
    process::{Command, Stdio},
    ptr,
};

const AX_SUCCESS: i32 = 0;
const K_CG_ANNOTATED_SESSION_EVENT_TAP: u32 = 2;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
const VIRTUAL_KEY_V: u16 = 0x09;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

const DIRECT_TYPE_THRESHOLD_CHARS: usize = 400;
const UNICODE_CHUNK_SIZE: usize = 48;

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;
type UniChar = u16;
type Boolean = u8;
type CGKeyCode = u16;
type CGEventSourceRef = *mut c_void;
type CGEventRef = *mut c_void;
type CGEventFlags = u64;
type CGEventTapLocation = u32;
type AXUIElementRef = *const c_void;
type AXError = i32;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtualKey: CGKeyCode,
        keyDown: Boolean,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        stringLength: CFIndex,
        unicodeString: *const UniChar,
    );
    fn CGEventPost(tap: CGEventTapLocation, event: CGEventRef);
    fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);

    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;

    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        cStr: *const i8,
        encoding: u32,
    ) -> CFStringRef;

    static kCFAllocatorDefault: CFAllocatorRef;
}

#[derive(Debug, Clone, Copy)]
pub enum InsertionMode {
    Auto,
    CopyOnly,
}

trait InsertionBackend {
    fn has_focused_input_target(&self) -> bool;
    fn type_unicode_text(&self, text: &str) -> Result<(), String>;
    fn write_text_to_clipboard(&self, text: &str) -> Result<(), String>;
    fn post_command_v(&self) -> Result<(), String>;
}

#[derive(Debug, Default)]
struct MacOsInsertionBackend;

impl InsertionBackend for MacOsInsertionBackend {
    fn has_focused_input_target(&self) -> bool {
        has_focused_input_target()
    }

    fn type_unicode_text(&self, text: &str) -> Result<(), String> {
        type_unicode_text(text)
    }

    fn write_text_to_clipboard(&self, text: &str) -> Result<(), String> {
        write_text_to_clipboard(text)
    }

    fn post_command_v(&self) -> Result<(), String> {
        post_command_v()
    }
}

#[derive(Debug, Default)]
pub struct TextInsertionService {
    backend: MacOsInsertionBackend,
}

impl TextInsertionService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_text(&self, text: &str) -> Result<(), String> {
        insert_text_with_backend(&self.backend, text, InsertionMode::Auto)
    }

    pub fn copy_to_clipboard(&self, text: &str) -> Result<(), String> {
        insert_text_with_backend(&self.backend, text, InsertionMode::CopyOnly)
    }

    pub fn insert_text_with_mode(&self, text: &str, mode: InsertionMode) -> Result<(), String> {
        insert_text_with_backend(&self.backend, text, mode)
    }
}

fn insert_text_with_backend<B: InsertionBackend>(
    backend: &B,
    text: &str,
    mode: InsertionMode,
) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }

    if matches!(mode, InsertionMode::CopyOnly) {
        return backend.write_text_to_clipboard(text);
    }

    let should_use_paste_fallback =
        text.chars().count() > DIRECT_TYPE_THRESHOLD_CHARS || !backend.has_focused_input_target();

    if should_use_paste_fallback {
        return paste_via_clipboard(backend, text);
    }

    match backend.type_unicode_text(text) {
        Ok(()) => Ok(()),
        Err(direct_error) => paste_via_clipboard(backend, text).map_err(|paste_error| {
            format!(
                "Direct insertion failed ({direct_error}); clipboard fallback failed ({paste_error})"
            )
        }),
    }
}

fn paste_via_clipboard<B: InsertionBackend>(backend: &B, text: &str) -> Result<(), String> {
    backend.write_text_to_clipboard(text)?;
    backend.post_command_v()
}

fn write_text_to_clipboard(text: &str) -> Result<(), String> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start pbcopy: {error}"))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "Failed to open pbcopy stdin".to_string())?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("Failed writing text to pbcopy: {error}"))?;
    }

    let status = child
        .wait()
        .map_err(|error| format!("Failed waiting for pbcopy: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("pbcopy exited with status: {status}"))
    }
}

fn has_focused_input_target() -> bool {
    const AX_FOCUSED_APPLICATION_ATTRIBUTE: &[u8] = b"AXFocusedApplication\0";
    const AX_FOCUSED_UI_ELEMENT_ATTRIBUTE: &[u8] = b"AXFocusedUIElement\0";

    unsafe {
        let system_wide = AXUIElementCreateSystemWide();
        if system_wide.is_null() {
            return false;
        }

        let focused_app_attribute = CFStringCreateWithCString(
            kCFAllocatorDefault,
            AX_FOCUSED_APPLICATION_ATTRIBUTE.as_ptr() as *const i8,
            K_CF_STRING_ENCODING_UTF8,
        );
        let focused_ui_element_attribute = CFStringCreateWithCString(
            kCFAllocatorDefault,
            AX_FOCUSED_UI_ELEMENT_ATTRIBUTE.as_ptr() as *const i8,
            K_CF_STRING_ENCODING_UTF8,
        );

        if focused_app_attribute.is_null() || focused_ui_element_attribute.is_null() {
            if !focused_app_attribute.is_null() {
                CFRelease(focused_app_attribute);
            }
            if !focused_ui_element_attribute.is_null() {
                CFRelease(focused_ui_element_attribute);
            }
            CFRelease(system_wide as CFTypeRef);
            return false;
        }

        let mut focused_app: CFTypeRef = ptr::null();
        let app_status =
            AXUIElementCopyAttributeValue(system_wide, focused_app_attribute, &mut focused_app);

        let mut focused_element: CFTypeRef = ptr::null();
        let element_status = AXUIElementCopyAttributeValue(
            system_wide,
            focused_ui_element_attribute,
            &mut focused_element,
        );

        if !focused_app.is_null() {
            CFRelease(focused_app);
        }
        if !focused_element.is_null() {
            CFRelease(focused_element);
        }
        CFRelease(focused_app_attribute);
        CFRelease(focused_ui_element_attribute);
        CFRelease(system_wide as CFTypeRef);

        app_status == AX_SUCCESS && element_status == AX_SUCCESS
    }
}

fn type_unicode_text(text: &str) -> Result<(), String> {
    let utf16: Vec<u16> = text.encode_utf16().collect();

    for chunk in utf16.chunks(UNICODE_CHUNK_SIZE) {
        post_unicode_keystroke(chunk, true)?;
        post_unicode_keystroke(chunk, false)?;
    }

    Ok(())
}

fn post_unicode_keystroke(chunk: &[u16], key_down: bool) -> Result<(), String> {
    unsafe {
        let event = CGEventCreateKeyboardEvent(ptr::null_mut(), 0, key_down as Boolean);
        if event.is_null() {
            return Err("Failed to create keyboard event".to_string());
        }

        CGEventKeyboardSetUnicodeString(event, chunk.len() as CFIndex, chunk.as_ptr());
        CGEventPost(K_CG_ANNOTATED_SESSION_EVENT_TAP, event);
        CFRelease(event as CFTypeRef);
    }

    Ok(())
}

fn post_command_v() -> Result<(), String> {
    unsafe {
        let key_down = CGEventCreateKeyboardEvent(ptr::null_mut(), VIRTUAL_KEY_V, true as Boolean);
        if key_down.is_null() {
            return Err("Failed to create key-down event for Cmd+V".to_string());
        }
        CGEventSetFlags(key_down, K_CG_EVENT_FLAG_MASK_COMMAND as CGEventFlags);
        CGEventPost(K_CG_ANNOTATED_SESSION_EVENT_TAP, key_down);
        CFRelease(key_down as CFTypeRef);

        let key_up = CGEventCreateKeyboardEvent(ptr::null_mut(), VIRTUAL_KEY_V, false as Boolean);
        if key_up.is_null() {
            return Err("Failed to create key-up event for Cmd+V".to_string());
        }
        CGEventSetFlags(key_up, K_CG_EVENT_FLAG_MASK_COMMAND as CGEventFlags);
        CGEventPost(K_CG_ANNOTATED_SESSION_EVENT_TAP, key_up);
        CFRelease(key_up as CFTypeRef);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{
        insert_text_with_backend, InsertionBackend, InsertionMode, DIRECT_TYPE_THRESHOLD_CHARS,
    };

    #[derive(Debug)]
    struct MockBackend {
        focused_input: bool,
        type_result: Result<(), String>,
        copy_result: Result<(), String>,
        paste_result: Result<(), String>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Default for MockBackend {
        fn default() -> Self {
            Self {
                focused_input: true,
                type_result: Ok(()),
                copy_result: Ok(()),
                paste_result: Ok(()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl MockBackend {
        fn call_order(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }
    }

    impl InsertionBackend for MockBackend {
        fn has_focused_input_target(&self) -> bool {
            self.calls.borrow_mut().push("focus_check");
            self.focused_input
        }

        fn type_unicode_text(&self, _text: &str) -> Result<(), String> {
            self.calls.borrow_mut().push("direct_type");
            self.type_result.clone()
        }

        fn write_text_to_clipboard(&self, _text: &str) -> Result<(), String> {
            self.calls.borrow_mut().push("copy");
            self.copy_result.clone()
        }

        fn post_command_v(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("paste");
            self.paste_result.clone()
        }
    }

    #[test]
    fn copy_only_mode_only_updates_clipboard() {
        let backend = MockBackend::default();

        let result = insert_text_with_backend(&backend, "hello", InsertionMode::CopyOnly);

        assert!(result.is_ok());
        assert_eq!(backend.call_order(), vec!["copy"]);
    }

    #[test]
    fn auto_mode_prefers_direct_typing_for_short_text_with_focus() {
        let backend = MockBackend::default();

        let result = insert_text_with_backend(&backend, "short text", InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(backend.call_order(), vec!["focus_check", "direct_type"]);
    }

    #[test]
    fn auto_mode_uses_clipboard_when_focus_not_available() {
        let backend = MockBackend {
            focused_input: false,
            ..Default::default()
        };

        let result = insert_text_with_backend(&backend, "hello", InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(backend.call_order(), vec!["focus_check", "copy", "paste"]);
    }

    #[test]
    fn auto_mode_uses_clipboard_for_long_text() {
        let backend = MockBackend::default();
        let text = "a".repeat(DIRECT_TYPE_THRESHOLD_CHARS + 1);

        let result = insert_text_with_backend(&backend, &text, InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(backend.call_order(), vec!["copy", "paste"]);
    }

    #[test]
    fn auto_mode_falls_back_to_clipboard_when_direct_typing_fails() {
        let backend = MockBackend {
            type_result: Err("direct failed".to_string()),
            ..Default::default()
        };

        let result = insert_text_with_backend(&backend, "hello", InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(
            backend.call_order(),
            vec!["focus_check", "direct_type", "copy", "paste"]
        );
    }

    #[test]
    fn returns_combined_error_when_direct_and_clipboard_paths_fail() {
        let backend = MockBackend {
            type_result: Err("direct failed".to_string()),
            copy_result: Err("copy failed".to_string()),
            ..Default::default()
        };

        let result = insert_text_with_backend(&backend, "hello", InsertionMode::Auto);

        assert!(result.is_err());
        assert_eq!(
            backend.call_order(),
            vec!["focus_check", "direct_type", "copy"]
        );
        let error = result.unwrap_err();
        assert!(error.contains("direct failed"));
        assert!(error.contains("copy failed"));
    }

    #[test]
    fn empty_text_is_noop() {
        let backend = MockBackend::default();

        let result = insert_text_with_backend(&backend, "", InsertionMode::Auto);

        assert!(result.is_ok());
        assert!(backend.call_order().is_empty());
    }
}
