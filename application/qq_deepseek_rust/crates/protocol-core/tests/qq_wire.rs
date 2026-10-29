// Run the production QQ wire parser's tests on the host without linking ESP-IDF.
// This file imports the same implementation, not a mirrored model of its logic.
#[allow(dead_code)]
#[path = "../../../src/bot/protocol.rs"]
mod qq_wire;

use qq_wire as protocol;

#[allow(dead_code)]
#[path = "../../../src/bot/vision.rs"]
mod vision;
