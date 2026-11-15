//! Point ids of the NIBE S735C-7 (firmware 4.6.x) used by the analysis tools.
//! Checked against `/api/v1/devices/0/points` of the installation.

pub const OUTDOOR_BT1: i64 = 4;
pub const EXHAUST_BT20: i64 = 22; // exhaust air, the heat source of the exhaust-air heat pump
pub const EXTRACT_BT21: i64 = 23; // air after the evaporator
pub const SUPPLY_BT12: i64 = 15;
pub const RETURN_BT3: i64 = 2547;
pub const HOTWATER_TOP_BT7: i64 = 11;
pub const HOTWATER_CHARGE_BT6: i64 = 12;
pub const FLOW_BF1: i64 = 58;
pub const COMPRESSOR_POWER_W: i64 = 1801;
pub const UNIT_POWER_W: i64 = 22130; // total power of the unit
pub const COMPRESSOR_HZ: i64 = 1799;
pub const COMPRESSOR_STATUS: i64 = 1973; // 1 = on
pub const DEGREE_MINUTES: i64 = 781;
pub const AIRFLOW_REAL: i64 = 5205; // m³/h
pub const FAN_PERCENT: i64 = 5952;
pub const LIFETIME_PRODUCTION_KWH: i64 = 28392;
pub const LIFETIME_CONSUMPTION_KWH: i64 = 28393;
pub const HEATING_COMPRESSOR_ONLY_KWH: i64 = 27378;
pub const HOTWATER_COMPRESSOR_ONLY_KWH: i64 = 27379;
pub const HEATING_INCL_ADDITIONAL_KWH: i64 = 27382;
pub const HOTWATER_INCL_ADDITIONAL_KWH: i64 = 27383;
