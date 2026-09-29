//! Which paths a worker may never write, and path comparison as the file system
//! sees it. The rule lives in `kgraf_core::paths`, so `kgraf-sandbox`'s
//! merge-back applies the same one without depending on this crate.

pub use kgraf_core::paths::{
    fold_name, fold_path, forbidden_path, is_build_config_path, is_protected_name, same_path,
};
