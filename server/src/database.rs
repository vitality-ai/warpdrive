//! database.rs
//! 
//! Legacy database module for backward compatibility.
//! This re-exports the Database struct from sqlite_store to maintain existing API.

pub use crate::sqlite_store::Database;