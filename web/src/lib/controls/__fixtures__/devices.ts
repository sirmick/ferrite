// Capability-schema fixtures used by optionsModel tests (#69).
//
// Hand-rolled rather than recorded from real hardware so the tests stay
// deterministic and don't depend on whichever Soapy plugin is installed
// on a given dev box. Shapes match `server/src/device.rs::DeviceCapabilities`.

import type { DeviceCapabilities } from '$lib/api/devices';

/** RTL2832U + R820T2 — single Rx channel, single antenna, single gain stage. */
export const RTLSDR_CAPS: DeviceCapabilities = {
  info: {
    driver: 'rtlsdr',
    label: 'Generic RTL2832U OEM',
    serial: '00000001',
    args: { driver: 'rtlsdr', serial: '00000001' },
  },
  // SoapySDR returns `RTLSDR` capitalised; fixtures mirror the real
  // casing so the preset-lookup code paths exercise the normaliser.
  driver_key: 'RTLSDR',
  hardware_key: 'RTL2832U',
  hardware_info: { tuner: 'R820T2' },
  rx_channels: [
    {
      index: 0,
      antennas: ['RX'],
      sample_rate_ranges_hz: [
        { min: 250_000, max: 250_000, step: null },
        { min: 1_024_000, max: 1_024_000, step: null },
        { min: 1_536_000, max: 1_536_000, step: null },
        { min: 1_792_000, max: 1_792_000, step: null },
        { min: 1_920_000, max: 1_920_000, step: null },
        { min: 2_048_000, max: 2_048_000, step: null },
        { min: 2_160_000, max: 2_160_000, step: null },
        { min: 2_400_000, max: 2_400_000, step: null },
        { min: 2_560_000, max: 2_560_000, step: null },
        { min: 2_880_000, max: 2_880_000, step: null },
        { min: 3_200_000, max: 3_200_000, step: null },
      ],
      bandwidth_ranges_hz: [],
      frequency_ranges_hz: [{ min: 24_000_000, max: 1_766_000_000, step: null }],
      frequency_components: [
        { name: 'RF', ranges_hz: [{ min: 24_000_000, max: 1_766_000_000, step: null }] },
      ],
      gains: [{ name: 'TUNER', range_db: { min: 0, max: 49.6, step: 0.1 } }],
      overall_gain_range_db: { min: 0, max: 49.6, step: 0.1 },
      has_agc: true,
    },
  ],
  settings: [
    {
      key: 'direct_samp',
      label: 'Direct Sampling',
      description: null,
      units: null,
      data_type: 'string',
      default: '0',
      range: null,
      options: [
        { value: '0', label: 'Off' },
        { value: '1', label: 'I-ADC' },
        { value: '2', label: 'Q-ADC' },
      ],
    },
  ],
};

/** SDRPlay RSPduo — wider frequency, multiple antennas, IF/RF gain split. */
export const SDRPLAY_CAPS: DeviceCapabilities = {
  info: {
    driver: 'sdrplay',
    label: 'RSPduo 1809071H07',
    serial: '1809071H07',
    args: { driver: 'sdrplay', serial: '1809071H07' },
  },
  // SoapySDR returns `SDRplay` capitalised; same reasoning as RTLSDR
  // above — test the real-world casing, not the preset-file casing.
  driver_key: 'SDRplay',
  hardware_key: 'RSPduo',
  hardware_info: { hwVer: '3' },
  rx_channels: [
    {
      index: 0,
      antennas: ['Tuner 1 50ohm', 'Tuner 1 Hi-Z', 'Tuner 2 50ohm'],
      sample_rate_ranges_hz: [{ min: 62_500, max: 10_660_000, step: null }],
      bandwidth_ranges_hz: [
        { min: 200_000, max: 200_000, step: null },
        { min: 300_000, max: 300_000, step: null },
        { min: 600_000, max: 600_000, step: null },
        { min: 1_536_000, max: 1_536_000, step: null },
        { min: 5_000_000, max: 5_000_000, step: null },
        { min: 6_000_000, max: 6_000_000, step: null },
        { min: 7_000_000, max: 7_000_000, step: null },
        { min: 8_000_000, max: 8_000_000, step: null },
      ],
      frequency_ranges_hz: [{ min: 1_000, max: 2_000_000_000, step: null }],
      frequency_components: [
        { name: 'RF', ranges_hz: [{ min: 1_000, max: 2_000_000_000, step: null }] },
      ],
      gains: [
        { name: 'IFGR', range_db: { min: 20, max: 59, step: 1 } },
        { name: 'RFGR', range_db: { min: 0, max: 9, step: 1 } },
      ],
      overall_gain_range_db: { min: 0, max: 68, step: 1 },
      has_agc: true,
    },
  ],
  settings: [
    {
      key: 'biasT_ctrl',
      label: 'BiasT Enable',
      description: 'BiasT Control',
      units: null,
      data_type: 'bool',
      default: 'true',
      range: null,
      options: [],
    },
    {
      key: 'agc_setpoint',
      label: 'AGC Setpoint',
      description: 'AGC target dBFS',
      units: 'dB',
      data_type: 'int',
      default: '-30',
      range: { min: -60, max: 0, step: null },
      options: [],
    },
  ],
};
