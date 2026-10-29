# GOOUUU ESP32-S3-CAM

ESP-Claw board definition for the GOOUUU ESP32-S3-CAM development board.

## Hardware

| Feature | Configuration |
|---|---|
| SoC module | ESP32-S3-WROOM-1-N16R8 |
| Flash / PSRAM | 16 MB QIO flash / 8 MB octal PSRAM |
| Camera | OV2640, 8-bit DVP, 20 MHz XCLK |
| Storage | microSD, 1-bit SDMMC |
| Status LED | One WS2812 on GPIO48 |
| Console | UART0 through CH340C, `/dev/ttyUSB0` on Linux |
| Native USB | GPIO20 D- / GPIO19 D+ |

The board has no on-board microphone, speaker, or display. The initial profile
therefore targets text and camera-enabled agents rather than voice interaction.

## Pin map

### Camera

| Signal | GPIO |
|---|---:|
| SCCB SDA / SCL | 4 / 5 |
| VSYNC / HREF | 6 / 7 |
| XCLK / PCLK | 15 / 13 |
| D0..D7 | 11, 9, 8, 10, 12, 18, 17, 16 |
| RESET / PWDN | Not connected (`-1`) |

### microSD

| Signal | GPIO |
|---|---:|
| CMD | 38 |
| CLK | 39 |
| D0 | 40 |

## Build

ESP-IDF v5.5.4 is recommended.

```bash
cd application/edge_agent
idf.py set-target esp32s3
idf.py bmgr -c ./boards -b goouuu_esp32_s3_cam
idf.py build
idf.py -p /dev/ttyUSB0 flash monitor
```

Before the first flash, preserve the original 16 MB image:

```bash
esptool.py --chip esp32s3 --port /dev/ttyUSB0 read_flash 0 0x1000000 backups/original-flash.bin
```

If automatic reset cannot enter the ROM bootloader, hold **BOOT**, tap
**RESET**, release **BOOT**, and retry.
