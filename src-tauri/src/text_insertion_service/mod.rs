use std::{
    ffi::c_void,
    io::Write,
    process::{Command, Stdio},
    ptr,
    thread::sleep,
    time::Duration,
};

const AX_SUCCESS: i32 = 0;
const K_CG_ANNOTATED_SESSION_EVENT_TAP: u32 = 2;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
const VIRTUAL_KEY_V: u16 = 0x09;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

const DIRECT_TYPE_THRESHOLD_CHARS: usize = 400;
const UNICODE_CHUNK_SIZE: usize = 48;
const PASTE_REGISTER_DELAY_MS: u64 = 75;

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
    fn read_text_from_clipboard(&self) -> Result<String, String>;
    fn write_text_to_clipboard(&self, text: &str) -> Result<(), String>;
    fn post_command_v(&self) -> Result<(), String>;
    fn wait_for_paste_to_register(&self);
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

    fn read_text_from_clipboard(&self) -> Result<String, String> {
        read_text_from_clipboard()
    }

    fn write_text_to_clipboard(&self, text: &str) -> Result<(), String> {
        write_text_to_clipboard(text)
    }

    fn post_command_v(&self) -> Result<(), String> {
        post_command_v()
    }

    fn wait_for_paste_to_register(&self) {
        wait_for_paste_to_register();
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
    let previous_clipboard = match backend.read_text_from_clipboard() {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            eprintln!("Failed to read clipboard before paste fallback: {error}");
            None
        }
    };

    backend.write_text_to_clipboard(text)?;
    let paste_result = backend.post_command_v();
    if paste_result.is_ok() {
        backend.wait_for_paste_to_register();
    }

    if let Some(previous_clipboard) = previous_clipboard {
        if let Err(error) = backend.write_text_to_clipboard(&previous_clipboard) {
            eprintln!("Failed to restore clipboard after paste fallback: {error}");
        }
    }

    paste_result
}

fn read_text_from_clipboard() -> Result<String, String> {
    let output = Command::new("pbpaste")
        .output()
        .map_err(|error| format!("Failed to start pbpaste: {error}"))?;

    if !output.status.success() {
        return Err(format!("pbpaste exited with status: {}", output.status));
    }

    String::from_utf8(output.stdout)
        .map_err(|error| format!("Clipboard is not UTF-8 text: {error}"))
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
    for chunk in utf16_chunks_preserving_char_boundaries(text, UNICODE_CHUNK_SIZE) {
        post_unicode_keystroke(&chunk, true)?;
        post_unicode_keystroke(&chunk, false)?;
    }

    Ok(())
}

fn utf16_chunks_preserving_char_boundaries(text: &str, max_units: usize) -> Vec<Vec<u16>> {
    if max_units == 0 {
        return Vec::new();
    }

    let mut chunks: Vec<Vec<u16>> = Vec::new();
    let mut current_chunk: Vec<u16> = Vec::with_capacity(max_units);

    for character in text.chars() {
        let mut character_utf16 = [0_u16; 2];
        let encoded_character = character.encode_utf16(&mut character_utf16);

        if current_chunk.len() + encoded_character.len() > max_units && !current_chunk.is_empty() {
            chunks.push(current_chunk);
            current_chunk = Vec::with_capacity(max_units);
        }

        current_chunk.extend_from_slice(encoded_character);
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
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

fn wait_for_paste_to_register() {
    sleep(Duration::from_millis(PASTE_REGISTER_DELAY_MS));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{
        insert_text_with_backend, utf16_chunks_preserving_char_boundaries, InsertionBackend,
        InsertionMode, DIRECT_TYPE_THRESHOLD_CHARS, UNICODE_CHUNK_SIZE,
    };

    #[derive(Debug)]
    struct MockBackend {
        focused_input: bool,
        type_result: Result<(), String>,
        copy_result: Result<(), String>,
        restore_result: Result<(), String>,
        paste_result: Result<(), String>,
        clipboard_read_result: Result<String, String>,
        calls: RefCell<Vec<&'static str>>,
        clipboard_writes: RefCell<Vec<String>>,
    }

    impl Default for MockBackend {
        fn default() -> Self {
            Self {
                focused_input: true,
                type_result: Ok(()),
                copy_result: Ok(()),
                restore_result: Ok(()),
                paste_result: Ok(()),
                clipboard_read_result: Ok("previous clipboard".to_string()),
                calls: RefCell::new(Vec::new()),
                clipboard_writes: RefCell::new(Vec::new()),
            }
        }
    }

    impl MockBackend {
        fn call_order(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }

        fn clipboard_writes(&self) -> Vec<String> {
            self.clipboard_writes.borrow().clone()
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

        fn read_text_from_clipboard(&self) -> Result<String, String> {
            self.calls.borrow_mut().push("clipboard_read");
            self.clipboard_read_result.clone()
        }

        fn write_text_to_clipboard(&self, text: &str) -> Result<(), String> {
            self.calls.borrow_mut().push("copy");
            let mut clipboard_writes = self.clipboard_writes.borrow_mut();
            let write_index = clipboard_writes.len();
            clipboard_writes.push(text.to_string());

            if write_index == 0 {
                self.copy_result.clone()
            } else {
                self.restore_result.clone()
            }
        }

        fn post_command_v(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("paste");
            self.paste_result.clone()
        }

        fn wait_for_paste_to_register(&self) {
            self.calls.borrow_mut().push("wait");
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
        assert_eq!(
            backend.call_order(),
            vec![
                "focus_check",
                "clipboard_read",
                "copy",
                "paste",
                "wait",
                "copy"
            ]
        );
        assert_eq!(
            backend.clipboard_writes(),
            vec!["hello".to_string(), "previous clipboard".to_string()]
        );
    }

    #[test]
    fn auto_mode_uses_clipboard_for_long_text() {
        let backend = MockBackend::default();
        let text = "a".repeat(DIRECT_TYPE_THRESHOLD_CHARS + 1);

        let result = insert_text_with_backend(&backend, &text, InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(
            backend.call_order(),
            vec!["clipboard_read", "copy", "paste", "wait", "copy"]
        );
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
            vec![
                "focus_check",
                "direct_type",
                "clipboard_read",
                "copy",
                "paste",
                "wait",
                "copy"
            ]
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
            vec!["focus_check", "direct_type", "clipboard_read", "copy"]
        );
        let error = result.unwrap_err();
        assert!(error.contains("direct failed"));
        assert!(error.contains("copy failed"));
    }

    #[test]
    fn paste_succeeds_even_when_clipboard_restore_fails() {
        let backend = MockBackend {
            focused_input: false,
            restore_result: Err("restore failed".to_string()),
            ..Default::default()
        };

        let result = insert_text_with_backend(&backend, "hello", InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(
            backend.call_order(),
            vec![
                "focus_check",
                "clipboard_read",
                "copy",
                "paste",
                "wait",
                "copy"
            ]
        );
        assert_eq!(
            backend.clipboard_writes(),
            vec!["hello".to_string(), "previous clipboard".to_string()]
        );
    }

    #[test]
    fn skips_clipboard_restore_if_capture_fails() {
        let backend = MockBackend {
            focused_input: false,
            clipboard_read_result: Err("read failed".to_string()),
            ..Default::default()
        };

        let result = insert_text_with_backend(&backend, "hello", InsertionMode::Auto);

        assert!(result.is_ok());
        assert_eq!(
            backend.call_order(),
            vec!["focus_check", "clipboard_read", "copy", "paste", "wait"]
        );
        assert_eq!(backend.clipboard_writes(), vec!["hello".to_string()]);
    }

    #[test]
    fn empty_text_is_noop() {
        let backend = MockBackend::default();

        let result = insert_text_with_backend(&backend, "", InsertionMode::Auto);

        assert!(result.is_ok());
        assert!(backend.call_order().is_empty());
    }

    #[test]
    fn utf16_chunking_preserves_non_bmp_characters() {
        let text = format!("{}{}{}", "a".repeat(UNICODE_CHUNK_SIZE - 1), "😀😀", "𐍈");
        let chunks = utf16_chunks_preserving_char_boundaries(&text, UNICODE_CHUNK_SIZE);

        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.len() <= UNICODE_CHUNK_SIZE));

        let flattened: Vec<u16> = chunks.into_iter().flatten().collect();
        let reconstructed = String::from_utf16(&flattened).expect("valid UTF-16 chunks");
        assert_eq!(reconstructed, text);
    }

    #[test]
    fn utf16_chunking_never_splits_surrogate_pairs() {
        let text = format!("{}{}", "a".repeat(UNICODE_CHUNK_SIZE - 1), "😀😀😀");
        let chunks = utf16_chunks_preserving_char_boundaries(&text, UNICODE_CHUNK_SIZE);

        assert!(chunks.iter().all(|chunk| {
            chunk
                .last()
                .is_none_or(|unit| !(0xD800..=0xDBFF).contains(unit))
        }));
    }
}
