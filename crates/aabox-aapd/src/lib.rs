//! AABox AAP source daemon — library side.
//!
//! The Android service-wrapper loads this as a cdylib over JNI.

pub mod channels;
pub mod control;
pub mod control_channel;
pub mod encrypted;
pub mod framing;
pub mod nav;
pub mod services;
pub mod tls;
pub mod tls_tunnel;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod kmsg;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod usb;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(target_os = "android")]
mod jni_bridge {
    use jni::objects::JClass;
    use jni::sys::jstring;
    use jni::JNIEnv;

    /// Hello-world JNI export — proves the bridge works end-to-end.
    /// Kotlin: `external fun nativeVersion(): String`
    #[no_mangle]
    pub extern "system" fn Java_app_aabox_aapd_NativeBridge_nativeVersion<'l>(
        env: JNIEnv<'l>,
        _class: JClass<'l>,
    ) -> jstring {
        let v = super::version();
        env.new_string(v).expect("new_string").into_raw()
    }
}
