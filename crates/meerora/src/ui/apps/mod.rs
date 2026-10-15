//! Apps of the agent view besides the chat, the editor, the terminal and the
//! browser (D11). Each draws its tile with `tile(ui, core, index, head, body)`
//! and returns true while it has keyboard focus. `head` is the tile head
//! right of the title and left of the close button, `body` the rest.

pub mod activity;
pub mod changes;
pub mod files;
pub mod git;
pub mod viewer;
