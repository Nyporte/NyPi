// Loads the board's WebAssembly (crates/nypi-board-ui). A file of its own, because the page
// allows no inline script.
import init from "/assets/board.js";
init({ module_or_path: "/assets/board_bg.wasm" });
