//! AABox AAP source daemon — library side.
//!
//! The Android service-wrapper loads this as a cdylib over JNI.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(target_os = "android")]
mod jni_bridge {
    use jni::objects::{JClass, JString};
    use jni::sys::jstring;
    use jni::JNIEnv;

    /// Hello-world JNI export — proves the bridge works end-to-end.
    /// Kotlin: `external fun nativeVersion(): String`
    #[no_mangle]
    pub extern "system" fn Java_app_aabox_aapd_NativeBridge_nativeVersion<'l>(
        mut env: JNIEnv<'l>,
        _class: JClass<'l>,
    ) -> jstring {
        let v = super::version();
        env.new_string(v).expect("new_string").into_raw()
    }
}
