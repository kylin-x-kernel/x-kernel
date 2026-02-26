#[cfg(feature = "ext4-rsext4")]
mod rsext4;

#[cfg(feature = "ext4-rsext4")]
pub use rsext4::Ext4Filesystem as Rsext4Filesystem;

#[cfg(feature = "ext4-lwext4")]
mod lwext4;

#[cfg(feature = "ext4-lwext4")]
pub use lwext4::Ext4Filesystem as Lwext4Filesystem;

#[cfg(feature = "ext4-ext4_rs")]
mod ext4_rs;

#[cfg(all(
    not(feature = "ext4-rsext4"),
    not(feature = "ext4-lwext4"),
    feature = "ext4-ext4_rs"
))]
pub use Ext4RsFilesystem as Ext4Filesystem;
#[cfg(all(not(feature = "ext4-rsext4"), feature = "ext4-lwext4"))]
pub use Lwext4Filesystem as Ext4Filesystem;
#[cfg(feature = "ext4-rsext4")]
pub use Rsext4Filesystem as Ext4Filesystem;
#[cfg(feature = "ext4-ext4_rs")]
pub use ext4_rs::Ext4Filesystem as Ext4RsFilesystem;
