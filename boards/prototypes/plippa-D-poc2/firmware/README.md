# Firmware

POC2 drives UI I/O from the Raspberry Pi (GPIO + AW9523 over I2C). There is no separate UI MCU firmware for this prototype.

Production boards that hand off buttons/encoders/LEDs to an MCU will place firmware trees here and share wire formats via `plippa-ui-proto` (stub until needed).
