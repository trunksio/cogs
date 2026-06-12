pub mod config;
pub mod index;
pub mod note;
pub mod parse;
pub mod resolve;
pub mod scan;

pub use config::{Vault, VaultConfig};
pub use index::VaultIndex;
pub use note::{Link, LinkSpan, ParsedNote, ParsedResource};
pub use resolve::{LinkResolver, Resolution};
pub use scan::{IndexState, VaultScanner};
