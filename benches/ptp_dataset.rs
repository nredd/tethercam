//! Benchmarks for PTP dataset decoding.
//!
//! `DevicePropDesc` is polled to track camera state, so decode cost is on a hot path.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use tethercam::ptp::{
    DeviceInfo, DevicePropCode, DevicePropDesc, EventCode, GetSet, OperationCode, PropertyForm,
    Value,
};

fn device_info() -> DeviceInfo {
    DeviceInfo {
        standard_version: 100,
        vendor_extension_id: 0x11,
        vendor_extension_version: 100,
        vendor_extension_desc: "example.org: 1.0".into(),
        operations_supported: (0x1001..=0x101C).map(OperationCode).collect(),
        events_supported: (0x4000..=0x400E).map(EventCode).collect(),
        device_properties_supported: (0x5001..=0x501F).map(DevicePropCode).collect(),
        capture_formats: vec![0x3801],
        image_formats: vec![0x3801, 0x3000],
        manufacturer: "Example Corp".into(),
        model: "Model 1".into(),
        device_version: "1.00".into(),
        serial_number: "SN000000".into(),
        ..DeviceInfo::default()
    }
}

fn prop_desc() -> DevicePropDesc {
    DevicePropDesc {
        code: DevicePropCode::EXPOSURE_TIME,
        get_set: GetSet::GetSet,
        factory_default: Value::UInt32(100),
        current: Value::UInt32(250),
        form: PropertyForm::Enumeration((0..64).map(|i| Value::UInt32(100 + i * 10)).collect()),
    }
}

fn dataset_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("ptp_dataset");
    if let Ok(wire) = device_info().to_bytes() {
        group.bench_function("device_info_decode", |b| {
            b.iter(|| DeviceInfo::decode(black_box(&wire)));
        });
    }
    if let Ok(wire) = prop_desc().to_bytes() {
        group.bench_function("device_prop_desc_decode", |b| {
            b.iter(|| DevicePropDesc::decode(black_box(&wire)));
        });
    }
    group.finish();
}

criterion_group!(benches, dataset_decode);
criterion_main!(benches);
