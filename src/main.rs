use std::{sync::Arc, time::Duration};

use anyhow::{self, bail, Result};
use embedded_hal::blocking::delay::DelayMs;
use embedded_svc::wifi::Wifi;
use embedded_svc::{
    mqtt::client::{
        Event::Received, Publish, QoS,
    },
    wifi::{
        ClientConfiguration, ClientConnectionStatus, ClientIpStatus, ClientStatus, Configuration,
        Status,
    },
};
use esp_idf_hal::{
    delay::FreeRtos,
    i2c::{config::MasterConfig, Master, MasterPins, I2C0},
    peripherals::Peripherals,
    prelude::*,
};
use esp_idf_svc::{
    log::EspLogger,
    mqtt::client::{EspMqttClient, MqttClientConfiguration},
    netif::EspNetifStack,
    nvs::EspDefaultNvs,
    sysloop::EspSysLoopStack,
    wifi::EspWifi,
};
use esp_idf_sys::*;

use log::{info, warn};
use serde::{Deserialize, Serialize};
use shtcx::{self, PowerMode};

#[toml_cfg::toml_config]
pub struct Config {
    #[default("test.mosquitto.org")]
    mqtt_host: &'static str,
    #[default("")]
    mqtt_user: &'static str,
    #[default("")]
    mqtt_pass: &'static str,
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
}
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MqttMeasurement {
    /// The measured temperature.
    pub temperature: f32,
    /// The measured humidity.
    pub humidity: f32,
}

fn main() -> anyhow::Result<()> {
    link_patches();

    EspLogger::initialize_default();

    let app_config = CONFIG;

    let peripherals = Peripherals::take().unwrap();

    let sda = peripherals.pins.gpio10;
    let scl = peripherals.pins.gpio8;

    let i2c = Master::<I2C0, _, _>::new(
        peripherals.i2c0,
        MasterPins { sda, scl },
        <MasterConfig as Default>::default().baudrate(400.kHz().into()),
    )?;

    let mut sht = shtcx::shtc3(i2c);
    let device_id = sht.device_identifier().unwrap();

    info!("Device ID SHTC3: {}", device_id);

    let netif_stack = Arc::new(EspNetifStack::new()?);
    let sys_loop_stack = Arc::new(EspSysLoopStack::new()?);
    let default_nvs = Arc::new(EspDefaultNvs::new()?);
    let _wifi = wifi(
        netif_stack.clone(),
        sys_loop_stack.clone(),
        default_nvs.clone(),
    )?;

    let mqtt_config = MqttClientConfiguration {
        client_id: Some("esp-rust-board-shtc3-mqtt"),
        keep_alive_interval: Some(Duration::from_secs(120)),
        ..Default::default()
    };

    let broker_url = if !app_config.mqtt_user.is_empty() {
        format!(
            "mqtt://{}:{}@{}",
            app_config.mqtt_user, app_config.mqtt_pass, app_config.mqtt_host
        )
    } else {
        format!("mqtt://{}", app_config.mqtt_host)
    };

    let mut client =
        EspMqttClient::new(
            broker_url,
            &mqtt_config,
            move |message_event| match message_event {
                Ok(Received(msg)) => info!("MQTT Message: {:?}", msg),
                _ => warn!("Received from MQTT: {:?}", message_event),
            },
        )?;

    loop {
        sht.start_measurement(PowerMode::NormalMode).unwrap();
        FreeRtos.delay_ms(100u32);
        let measurement = sht.get_measurement_result().unwrap();

        info!(
            "TEMP: {} °C\nHUM: {:?} %\n",
            measurement.temperature.as_degrees_celsius(),
            measurement.humidity.as_percent(),
        );

        let m = MqttMeasurement {
            temperature: measurement.temperature.as_degrees_celsius(),
            humidity: measurement.humidity.as_percent(),
        };

        let js = serde_json::to_string(&m)?;
        client.publish(
            &format!("{}/feeds/measurement", app_config.mqtt_user),
            QoS::AtMostOnce,
            false,
            js.as_bytes(),
        )?;
        info!("Published message: {}", js);

        FreeRtos.delay_ms(500u32);
    }
}

fn wifi(
    netif_stack: Arc<EspNetifStack>,
    sys_loop_stack: Arc<EspSysLoopStack>,
    default_nvs: Arc<EspDefaultNvs>,
) -> Result<Box<EspWifi>> {
    let mut wifi = Box::new(EspWifi::new(netif_stack, sys_loop_stack, default_nvs)?);

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: CONFIG.wifi_ssid.into(),
        password: CONFIG.wifi_psk.into(),
        ..Default::default()
    }))?;

    info!("Wifi configuration set, about to get status");

    wifi.wait_status_with_timeout(Duration::from_secs(20), |status| !status.is_transitional())
        .map_err(|e| anyhow::anyhow!("Unexpected Wifi status: {:?}", e))?;

    let status = wifi.get_status();

    if let Status(
        ClientStatus::Started(ClientConnectionStatus::Connected(ClientIpStatus::Done(
            _ip_settings,
        ))),
        _,
    ) = status
    {
        info!("Wifi connected");
    } else {
        bail!("Unexpected Wifi status: {:?}", status);
    }

    Ok(wifi)
}
