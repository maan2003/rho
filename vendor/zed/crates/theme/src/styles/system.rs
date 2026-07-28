#![allow(missing_docs)]

use gpui::{Color, hsla};

#[derive(Clone, Debug, PartialEq)]
pub struct SystemColors {
    pub transparent: Color,
    pub mac_os_traffic_light_red: Color,
    pub mac_os_traffic_light_yellow: Color,
    pub mac_os_traffic_light_green: Color,
}

impl Default for SystemColors {
    fn default() -> Self {
        Self {
            transparent: hsla(0.0, 0.0, 0.0, 0.0).into(),
            mac_os_traffic_light_red: hsla(0.0139, 0.79, 0.65, 1.0).into(),
            mac_os_traffic_light_yellow: hsla(0.114, 0.88, 0.63, 1.0).into(),
            mac_os_traffic_light_green: hsla(0.313, 0.49, 0.55, 1.0).into(),
        }
    }
}
