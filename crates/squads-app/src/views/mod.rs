//! The egui screens (milestone **M10**).
//!
//! One module per screen. Every view is a function taking `&mut SquadsApp` and
//! `&mut egui::Ui`; it reads state through the app's accessors and records user
//! intent with [`SquadsApp::push_action`]. Nothing here talks to the network or
//! mutates the core directly — [`crate::app::SquadsApp::apply_actions`] runs
//! after the frame is drawn.

pub mod call;
pub mod chat_list;
pub mod composer;
pub mod conversation;
pub mod files;
pub mod login;
pub mod settings;
pub mod shell;
pub mod teams;
