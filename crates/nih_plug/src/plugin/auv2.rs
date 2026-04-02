use super::Plugin;

/// Additional metadata needed to expose a plugin as an AUv2 component on macOS.
pub trait Auv2Plugin: Plugin {
    /// The component type. Audio effects should use `*b"aufx"`.
    const AUV2_TYPE: [u8; 4] = *b"aufx";
    /// The globally unique subtype for this plugin.
    const AUV2_SUBTYPE: [u8; 4];
    /// The 4-character manufacturer code.
    const AUV2_MANUFACTURER: [u8; 4];
    /// Whether this component can be loaded directly inside sandboxed hosts.
    const AUV2_SANDBOX_SAFE: bool = true;
}
