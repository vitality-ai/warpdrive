//! Who requests run as, shared by every front end.

/// The single static admin identity's canonical user ID. Every bucket/object in
/// admin-only mode is owned by this ID — resource-owner lookups use this constant
/// rather than a literal so the one place that assumes "single owner" is easy to
/// find and replace once Vitality Console (UAM) can resolve real per-bucket owners.
pub const ADMIN_USER_ID: &str = "admin";
