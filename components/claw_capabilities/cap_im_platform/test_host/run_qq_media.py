#!/usr/bin/env python3
"""Compile the firmware's actual QQ media walker with cJSON on the host.

Extract functions by their source boundaries to avoid stubbing the entire
FreeRTOS/WebSocket transport or maintaining a second parser implementation.
"""
import pathlib
import subprocess
import tempfile

here = pathlib.Path(__file__).resolve().parent
repo = here.parents[3]
cjson = repo / "application/edge_agent/managed_components/espressif__cjson/cJSON"
source = (here.parent / "src/cap_im_qq.c").read_text()


def block(start, end):
    return source[source.index(start):source.index(end, source.index(start))]


implementation = "\n".join([
    '#include <stdbool.h>\n#include <string.h>\n#include <strings.h>\n#include "cJSON.h"',
    block("static bool cap_im_qq_is_image_mime(", "static bool cap_im_qq_is_audio_mime("),
    block("static const char *cap_im_qq_get_object_string(cJSON *object, const char *name)\n{",
          "static void cap_im_qq_extract_quote_id("),
    block("static bool cap_im_qq_url_looks_like_image(",
          "static bool cap_im_qq_group_content_mentions_bot("),
    (here / "test_qq_media.c").read_text(),
])
with tempfile.TemporaryDirectory(prefix="esp-qq-media-test-") as directory:
    unit = pathlib.Path(directory) / "qq_media.c"
    unit.write_text(implementation)
    exe = pathlib.Path(directory) / "test_qq_media"
    subprocess.run([
        "cc", "-std=gnu11", "-D_DEFAULT_SOURCE", "-Wall", "-Wextra", "-Werror", "-g",
        "-fsanitize=address,undefined", "-fno-omit-frame-pointer",
        "-I", str(cjson), str(unit), str(cjson / "cJSON.c"),
        "-lm", "-o", str(exe),
    ], check=True)
    subprocess.run([str(exe)], check=True)
