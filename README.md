## Manual Hook Check

Task 13 provides the low-level hook API in `borderless-win`; `borderless-app` runtime integration is not wired yet. After that integration is added, run `cargo run -p borderless-app`, choose Controller, press Start, then move the mouse and press keys while the event log is visible. The log should show pointer position and input event activity. Stop should restore normal local input behavior immediately.
