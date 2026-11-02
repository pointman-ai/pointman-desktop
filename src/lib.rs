//! agent-desktop (github.com/vercel-labs/agent-desktop) on Pointman's machines: `install` puts its pinned release
//! in the extension's state folder, `cli` runs one call and reads its answer, and `session` finds what a Linux
//! desktop's programs need that the node's service doesn't have.

pub mod cli;
pub mod install;
pub mod session;
