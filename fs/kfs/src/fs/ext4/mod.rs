#[cfg(feature = "ext4-rsext4")]
mod rsext4;

#[cfg(feature = "ext4-rsext4")]
pub use rsext4::Ext4Filesystem;

#[cfg(feature = "ext4-lwext4")]
mod lwext4;

#[cfg(feature = "ext4-lwext4")]
pub use lwext4::Ext4Filesystem;

#[cfg(feature = "ext4-ext4_rs")]
mod ext4_rs;

#[cfg(feature = "ext4-ext4_rs")]
pub use ext4_rs::Ext4Filesystem;
