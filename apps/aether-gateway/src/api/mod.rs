pub(crate) mod ai;
pub(crate) mod backend;
mod core;
mod ops;
pub(crate) mod response;

pub(crate) use ai::mount_ai_routes;
pub(crate) use backend::{
    mount_admin_routes, mount_internal_routes, mount_oauth_routes, mount_public_support_routes,
};
pub(crate) use core::mount_core_routes;
pub(crate) use ops::{mount_disabled_operational_routes, mount_operational_routes};
