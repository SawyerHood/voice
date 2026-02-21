use serde::{Deserialize, Serialize};

const MICROPHONE_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

const AV_AUTHORIZATION_STATUS_NOT_DETERMINED: i64 = 0;
const AV_AUTHORIZATION_STATUS_RESTRICTED: i64 = 1;
const AV_AUTHORIZATION_STATUS_DENIED: i64 = 2;
const AV_AUTHORIZATION_STATUS_AUTHORIZED: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    NotDetermined,
    Granted,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionType {
    Microphone,
    Accessibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSnapshot {
    pub microphone: PermissionState,
    pub accessibility: PermissionState,
    pub all_granted: bool,
}

impl PermissionSnapshot {
    fn new(microphone: PermissionState, accessibility: PermissionState) -> Self {
        Self {
            microphone,
            accessibility,
            all_granted: microphone == PermissionState::Granted
                && accessibility == PermissionState::Granted,
        }
    }
}

#[derive(Debug, Default)]
pub struct PermissionService;

impl PermissionService {
    pub fn new() -> Self {
        Self
    }

    pub fn check_permissions(&self) -> PermissionSnapshot {
        let microphone = self.microphone_permission();
        let accessibility = self.accessibility_permission();
        PermissionSnapshot::new(microphone, accessibility)
    }

    pub fn microphone_permission(&self) -> PermissionState {
        #[cfg(target_os = "macos")]
        {
            return macos::microphone_permission();
        }

        #[cfg(not(target_os = "macos"))]
        {
            PermissionState::Granted
        }
    }

    pub fn accessibility_permission(&self) -> PermissionState {
        #[cfg(target_os = "macos")]
        {
            return macos::accessibility_permission();
        }

        #[cfg(not(target_os = "macos"))]
        {
            PermissionState::Granted
        }
    }

    pub fn request_permission(
        &self,
        permission_type: PermissionType,
    ) -> Result<PermissionSnapshot, String> {
        #[cfg(target_os = "macos")]
        {
            match permission_type {
                PermissionType::Microphone => {
                    let status = macos::request_microphone_permission()?;
                    if status != PermissionState::Granted {
                        open_system_settings(MICROPHONE_SETTINGS_URL)?;
                    }
                }
                PermissionType::Accessibility => {
                    let trusted = macos::request_accessibility_permission();
                    if !trusted {
                        open_system_settings(ACCESSIBILITY_SETTINGS_URL)?;
                    }
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = permission_type;
        }

        Ok(self.check_permissions())
    }
}

pub fn map_microphone_authorization_status(status: i64) -> PermissionState {
    match status {
        AV_AUTHORIZATION_STATUS_AUTHORIZED => PermissionState::Granted,
        AV_AUTHORIZATION_STATUS_NOT_DETERMINED => PermissionState::NotDetermined,
        AV_AUTHORIZATION_STATUS_RESTRICTED | AV_AUTHORIZATION_STATUS_DENIED => {
            PermissionState::Denied
        }
        _ => PermissionState::Denied,
    }
}

#[cfg(target_os = "macos")]
fn open_system_settings(url: &str) -> Result<(), String> {
    use std::process::Command;

    let status = Command::new("open")
        .arg(url)
        .status()
        .map_err(|error| format!("Failed to open System Settings: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "System Settings exited with status {status} while opening permission pane"
        ))
    }
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
mod macos {
    use std::{ffi::c_void, ptr, sync::mpsc, time::Duration};

    use block::ConcreteBlock;
    use objc::{class, msg_send, runtime::BOOL, sel, sel_impl};

    use super::{map_microphone_authorization_status, PermissionState};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFMutableDictionaryRef = *const c_void;
    type CFIndex = isize;
    type Boolean = u8;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> Boolean;
        fn AXIsProcessTrustedWithOptions(the_dict: CFDictionaryRef) -> Boolean;

        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: CFStringRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryCreateMutable(
            allocator: CFAllocatorRef,
            capacity: CFIndex,
            key_call_backs: *const c_void,
            value_call_backs: *const c_void,
        ) -> CFMutableDictionaryRef;
        fn CFDictionarySetValue(
            the_dict: CFMutableDictionaryRef,
            key: *const c_void,
            value: *const c_void,
        );

        static kCFBooleanTrue: CFTypeRef;

        fn CFRelease(cf: CFTypeRef);
    }

    pub(super) fn microphone_permission() -> PermissionState {
        unsafe {
            let capture_device_class = class!(AVCaptureDevice);
            let authorization_status: i64 =
                msg_send![capture_device_class, authorizationStatusForMediaType: AVMediaTypeAudio];
            map_microphone_authorization_status(authorization_status)
        }
    }

    pub(super) fn request_microphone_permission() -> Result<PermissionState, String> {
        let current_status = microphone_permission();
        if current_status != PermissionState::NotDetermined {
            return Ok(current_status);
        }

        let (tx, rx) = mpsc::channel::<bool>();

        unsafe {
            let capture_device_class = class!(AVCaptureDevice);
            let completion = ConcreteBlock::new(move |granted: BOOL| {
                let _ = tx.send(granted);
            })
            .copy();

            let _: () = msg_send![
                capture_device_class,
                requestAccessForMediaType: AVMediaTypeAudio
                completionHandler: &*completion
            ];

            let received = rx.recv_timeout(Duration::from_secs(20)).ok();
            drop(completion);

            match received {
                Some(true) => Ok(PermissionState::Granted),
                Some(false) => Ok(PermissionState::Denied),
                None => Ok(microphone_permission()),
            }
        }
    }

    pub(super) fn accessibility_permission() -> PermissionState {
        if unsafe { AXIsProcessTrusted() != 0 } {
            PermissionState::Granted
        } else {
            PermissionState::Denied
        }
    }

    pub(super) fn request_accessibility_permission() -> bool {
        unsafe {
            let options = CFDictionaryCreateMutable(ptr::null(), 1, ptr::null(), ptr::null());
            if options.is_null() {
                return AXIsProcessTrusted() != 0;
            }

            CFDictionarySetValue(
                options,
                kAXTrustedCheckOptionPrompt as *const c_void,
                kCFBooleanTrue as *const c_void,
            );

            let trusted = AXIsProcessTrustedWithOptions(options as CFDictionaryRef) != 0;
            CFRelease(options as CFTypeRef);
            trusted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{map_microphone_authorization_status, PermissionSnapshot, PermissionState};

    #[test]
    fn maps_microphone_status_to_not_determined() {
        assert_eq!(
            map_microphone_authorization_status(0),
            PermissionState::NotDetermined
        );
    }

    #[test]
    fn maps_microphone_status_to_granted() {
        assert_eq!(
            map_microphone_authorization_status(3),
            PermissionState::Granted
        );
    }

    #[test]
    fn maps_restricted_and_denied_microphone_status_to_denied() {
        assert_eq!(
            map_microphone_authorization_status(1),
            PermissionState::Denied
        );
        assert_eq!(
            map_microphone_authorization_status(2),
            PermissionState::Denied
        );
        assert_eq!(
            map_microphone_authorization_status(42),
            PermissionState::Denied
        );
    }

    #[test]
    fn permission_snapshot_reports_all_granted_only_when_both_permissions_are_granted() {
        let all_granted =
            PermissionSnapshot::new(PermissionState::Granted, PermissionState::Granted);
        let missing_mic =
            PermissionSnapshot::new(PermissionState::Denied, PermissionState::Granted);
        let missing_accessibility =
            PermissionSnapshot::new(PermissionState::Granted, PermissionState::Denied);

        assert!(all_granted.all_granted);
        assert!(!missing_mic.all_granted);
        assert!(!missing_accessibility.all_granted);
    }
}
