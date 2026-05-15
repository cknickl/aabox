//! On-device CarCar re-signer service — library side.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// TODO: dex patcher — find sget Build$VERSION;->SDK_INT:I followed by packed-switch
//       followed by throw "Unsupported android version" / "Unsupported Android version".
//       Clamp the register to 0x23 (35 decimal).
//
// TODO: apksigner-equivalent — embed AABox platform.pk8 + .x509.pem, sign with v2+v3
//       schemes. Crates: ring + asn1-rs + manual zipalign-aware APK manipulation,
//       OR call into a Rust port of apksig (none exists yet — may need to ship a
//       small portion of the AOSP apksig sources translated).
//
// TODO: PackageInstaller hook — register as a SessionCallback. On
//       INSTALL_FAILED_UPDATE_INCOMPATIBLE for com.example.car_launcher, pull
//       staged APK, patch + re-sign, submit new install session.
