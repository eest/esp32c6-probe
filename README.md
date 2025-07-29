# esp32c6-probe

Playing around with programming an esp32c6 device to connect to WiFi, getting
an IPv4 address via DHCP and then continously reading temperature/humidity
values from a sensor and publishing these results to an MQTT server.

Specifically the code is tested against a `ESP32-C6-DevKitC 8MB` board
connected to a `HDC3022` sensor. The physical setup on a breadbord can be seen
at https://github.com/eest/esp32c6-hdc3022.

To flash the device after plugging it in via USB-C:
```
WIFI_SSID=wifi_ssid WIFI_PASSWORD=wifi_password MQTT_SERVER_IPV4=192.0.2.0 MQTT_USERNAME=mqtt_username MQTT_PASSWORD=mqtt_password cargo run
```
