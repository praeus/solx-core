wit_bindgen::generate!({
    world: "custom-action",
    path: "wit",
});

pub use sol::actions::action_exec::exec as exec_action;
pub use sol::actions::artifact_read::read as read_artifact;
pub use sol::actions::logger::log;
pub use exports::sol::actions::runner::{ActionResult, Guest};
