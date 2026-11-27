#![allow(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    clippy::unwrap_used
)]

use crate::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(
        Btrt{
            buffer_size_db: 0x1234_5678,
            max_bitrate: 0x3456_789a,
            avg_bitrate: 0x5678_9abc,
        },
        &[
            0x12, 0x34, 0x56, 0x78, // buffer_size_db.
            0x34, 0x56, 0x78, 0x9a, // max_bitrate.
            0x56, 0x78, 0x9a, 0xbc, // avg_bitrate.
        ]; "btrt"
    )]
#[test_case(
        Ctts{
            flags: [0, 0, 0],
            entries: CttsEntries::V0(vec![
                CttsEntryV0{
                    sample_count: 0x0123_4567,
                    sample_offset: 0x1234_5678,
                },
                CttsEntryV0{
                    sample_count: 0x89ab_cdef,
                    sample_offset: 0x789a_bcde,
                },
            ]),
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x02, // entry count
            0x01, 0x23, 0x45, 0x67, // sample count
            0x12, 0x34, 0x56, 0x78, // sample offset
            0x89, 0xab, 0xcd, 0xef, // sample count
            0x78, 0x9a, 0xbc, 0xde, // sample offset
        ]; "ctts: version 0"
    )]
#[test_case(
        Ctts{
            flags: [0, 0, 0],
            entries: CttsEntries::V1(vec![
                CttsEntryV1{
                    sample_count: 0x0123_4567,
                    sample_offset: 0x1234_5678,
                },
                CttsEntryV1{
                    sample_count: 0x89ab_cdef,
                    sample_offset: -0x789a_bcde,
                },
            ]),
        },
        &[
            1,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x02, // entry count
            0x01, 0x23, 0x45, 0x67, // sample count
            0x12, 0x34, 0x56, 0x78, // sample offset
            0x89, 0xab, 0xcd, 0xef, // sample count
            0x87, 0x65, 0x43, 0x22, // sample offset
        ]; "ctts: version 1"
    )]
#[test_case(Dinf{}, &[]; "dinf")]
#[test_case(
        Dref{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            entry_count: 0x1234_5678,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x12, 0x34, 0x56, 0x78, // entry count
        ]; "dref"
    )]
#[test_case(
        Url{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 1],
            },
            location: String::new(),
        },
        &[
            0,                // version
            0x00, 0x00, 0x01, // flags
        ]; "url"
    )]
#[test_case(
        Ftyp{
            major_brand:   [b'a', b'b', b'e', b'm'],
            minor_version: 0x1234_5678,
            compatible_brands: vec![
                CompatibleBrandElem(*b"abcd"),
                CompatibleBrandElem(*b"efgh"),
            ],
        },
        &[
            b'a', b'b', b'e', b'm', // major brand
            0x12, 0x34, 0x56, 0x78, // minor version
            b'a', b'b', b'c', b'd', // compatible brand
            b'e', b'f', b'g', b'h', // compatible brand
        ]; "ftyp"
    )]
#[test_case(
        Hdlr{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            pre_defined: 0x1234_5678,
            handler_type: *b"abem",
            reserved: [0, 0, 0],
            name: "Abema".to_owned(),
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x12, 0x34, 0x56, 0x78, // pre-defined
            b'a', b'b', b'e', b'm', // handler type
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // reserved
            b'A', b'b', b'e', b'm', b'a', 0x00, // name
        ]; "hdlr"
    )]
#[test_case(
        Hdlr{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            pre_defined: 0,
            handler_type: *b"vide",
            reserved: [0, 0, 0],
            name: "VideoHandler".to_owned(),
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x00, // pre-defined
            b'v', b'i', b'd', b'e', // handler type
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, // reserved
            b'V', b'i', b'd', b'e', b'o', b'H', b'a', b'n', b'd', b'l', b'e', b'r', 0x00, // name
        ];"hdlr2"
    )]
#[test_case(
        Mdhd{
            flags: [0, 0, 0],
            version: MdhdVersion::V0(MdhdV0{
                creation_time: 0x1234_5678,
                modification_time: 0x2345_6789,
                duration: 0x0203_0405,
            }),
            timescale: 0x0102_0304,
            pad: true,
            language: [b'j' - 0x60, b'p' - 0x60, b'n' - 0x60], // 0x0a, 0x10, 0x0e
            pre_defined: 0,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x12, 0x34, 0x56, 0x78, // creation time
            0x23, 0x45, 0x67, 0x89, // modification time
            0x01, 0x02, 0x03, 0x04, // timescale
            0x02, 0x03, 0x04, 0x05, // duration
            0xaa, 0x0e, // pad, language (1 01010 10000 01110)
            0x00, 0x00, // pre defined
        ]; "mdhd: version 0"
    )]
#[test_case(
        Mdhd{
            flags: [0, 0, 0],
            version: MdhdVersion::V1(MdhdV1{
                creation_time: 0x1234_5678_9abc_def0,
                modification_time: 0x2345_6789_abcd_ef01,
                duration: 0x0203_0405_0607_0809,
            }),
            timescale: 0x0102_0304,
            pad: true,
            language: [b'j' - 0x60, b'p' - 0x60, b'n' - 0x60], // 0x0a, 0x10, 0x0e
            pre_defined: 0,
        },
        &[
            1,                // version
            0x00, 0x00, 0x00, // flags
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, // creation time
            0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, // modification time
            0x01, 0x02, 0x03, 0x04, // timescale
            0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, // duration
            0xaa, 0x0e, // pad, language (1 01010 10000 01110)
            0x00, 0x00, // pre defined
        ]; "mdhd: version 1"
    )]
#[test_case(
        Mdhd{
            flags: [0, 0, 0],
            version: MdhdVersion::V0(MdhdV0{
                creation_time: 0,
                modification_time: 0,
                duration: 0,
            }),
            timescale: 0x0102_0304,
            pad: false,
            language: *b"und",
            pre_defined: 0,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x0, 0x0, 0x0, 0x0, // creation time
            0x0, 0x0, 0x0, 0x0, // modification time
            0x01, 0x02, 0x03, 0x04, // timescale
            0x00, 0x00, 0x00, 0x00, // duration
            0x55, 0xc4, // pad, language
            0x00, 0x00, // pre defined
        ]; "mdhd: language"
    )]
#[test_case(Mdia{}, &[]; "mdia")]
#[test_case(
        Mfhd{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            sequence_number: 0x1234_5678,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x12, 0x34, 0x56, 0x78, // sequence number
        ]; "mfhd"
    )]
#[test_case(Minf{}, &[]; "minf")]
#[test_case(Moof{}, &[]; "moof")]
#[test_case(Moov{}, &[]; "moov")]
#[test_case(Mvex{}, &[]; "mvex")]
#[test_case(
        Mvhd{
            flags: [0, 0, 0],
            version: MvhdVersion::V0(MvhdV0{
                creation_time: 0x0123_4567,
                modification_time: 0x2345_6789,
                duration: 0x6789_abcd,
            }),
            timescale: 0x4567_89ab,
            rate: -0x0123_4567,
            volume: 0x0123,
            reserved: 0,
            reserved2: [0; 2],
            matrix: [0; 9],
            pre_defined: [0;6],
            next_track_id: 0xabcd_ef01,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, // creation time
            0x23, 0x45, 0x67, 0x89, // modification time
            0x45, 0x67, 0x89, 0xab, // timescale
            0x67, 0x89, 0xab, 0xcd, // duration
            0xfe, 0xdc, 0xba, 0x99, // rate
            0x01, 0x23, // volume
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // matrix
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // pre-defined
            0xab, 0xcd, 0xef, 0x01, // next track ID
        ]; "mvhd: version 0"
    )]
#[test_case(
        Mvhd{
            flags: [0, 0, 0],
            version: MvhdVersion::V1(MvhdV1{
                creation_time: 0x0123_4567_89ab_cdef,
                modification_time: 0x2345_6789_abcd_ef01,
                duration: 0x4567_89ab_cdef_0123,
            }),
            timescale: 0x89ab_cdef,
            rate: -0x0123_4567,
            volume: 0x0123,
            reserved: 0,
            reserved2: [0; 2],
            matrix: [0; 9],
            pre_defined: [0; 6],
            next_track_id: 0xabcd_ef01,
        },
        &[
            1,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, // creation time
            0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, // modification
            0x89, 0xab, 0xcd, 0xef, // timescale
            0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, // duration
            0xfe, 0xdc, 0xba, 0x99, // rate
            0x01, 0x23, // volume
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // matrix
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // pre-defined
            0xab, 0xcd, 0xef, 0x01, // next track ID
        ]; "mvhd: version 1"
    )]
#[test_case(
        Avc1{
            sample_entry: SampleEntry{
                reserved: [0; 6],
                data_reference_index: 0x1234,
            },
            pre_defined: 0x0101,
            pre_defined2: [0x0100_0001, 0x0100_0002, 0x0100_0003],
            reserved: 0,
            width: 0x0102,
            height: 0x0103,
            horiz_resolution: 0x0100_0004,
            vert_resolution: 0x0100_0005,
            reserved2: 0x0100_0006,
            frame_count: 0x0104,
            compressor_name: [8, b'a', b'b', b'e', b'm', b'a', 0x00, b't', b'v',
                0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
            depth: 0x0105,
            pre_defined3: 1001,
        },
        &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            0x12, 0x34, // data reference index
            0x01, 0x01, // pre_defined
            0x00, 0x00, // reserved
            0x01, 0x00, 0x00, 0x01,
            0x01, 0x00, 0x00, 0x02,
            0x01, 0x00, 0x00, 0x03, // pre_defined2
            0x01, 0x02, // width
            0x01, 0x03, // height
            0x01, 0x00, 0x00, 0x04, // horiz_resolution
            0x01, 0x00, 0x00, 0x05, // vert_resolution
            0x01, 0x00, 0x00, 0x06, // reserved2
            0x01, 0x04, // frame_count
            8, b'a', b'b', b'e', b'm', b'a', 0x00, b't',
            b'v', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // compressor_name
            0x01, 0x05, // depth
            0x03, 0xe9, // pre_defined3
        ]; "Avc1"
    )]
#[test_case(Stbl{}, &[]; "stbl")]
#[test_case(
        Stco{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            chunk_offsets: vec![0x0123_4567, 0x89ab_cdef],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x02, // entry count
            0x01, 0x23, 0x45, 0x67, // chunk offset
            0x89, 0xab, 0xcd, 0xef, // chunk offset
        ]; "stco"
    )]
#[test_case(
        Stsc{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            entries: vec![
                StscEntry{first_chunk: 0x0123_4567, samples_per_chunk: 0x2345_6789, sample_description_index: 0x4567_89ab},
                StscEntry{first_chunk: 0x6789_abcd, samples_per_chunk: 0x89ab_cdef, sample_description_index: 0xabcd_ef01},
            ],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x02, // entry count
            0x01, 0x23, 0x45, 0x67, // first chunk
            0x23, 0x45, 0x67, 0x89, // sample per chunk
            0x45, 0x67, 0x89, 0xab, // sample description index
            0x67, 0x89, 0xab, 0xcd, // first chunk
            0x89, 0xab, 0xcd, 0xef, // sample per chunk
            0xab, 0xcd, 0xef, 0x01, // sample description index
        ]; "stsc"
    )]
#[test_case(
        Stsd{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            entry_count: 0x0123_4567,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, // entry count
        ]; "stsd"
    )]
#[test_case(
        Stss{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            sample_numbers: vec![0x0123_4567, 0x89ab_cdef],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x02, // entry count
            0x01, 0x23, 0x45, 0x67, // sample number
            0x89, 0xab, 0xcd, 0xef, // sample number
        ]; "stss"
    )]
#[test_case(
        Stsz{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            sample_size:  0x0123_4567,
            sample_count: 2,
            entry_sizes: vec![],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, // sample size
            0x00, 0x00, 0x00, 0x02, // sample count
        ]; "stsz: common sample size"
    )]
#[test_case(
        Stsz{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            sample_size: 0,
            sample_count: 2,
            entry_sizes:  vec![0x0123_4567, 0x2345_6789],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x00, // sample size
            0x00, 0x00, 0x00, 0x02, // sample count
            0x01, 0x23, 0x45, 0x67, // entry size
            0x23, 0x45, 0x67, 0x89, // entry size
        ]; "stsz: sample size array"
    )]
#[test_case(
        Stts{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            entries: vec![
                SttsEntry{sample_count: 0x0123_4567, sample_delta: 0x2345_6789},
                SttsEntry{sample_count: 0x4567_89ab, sample_delta: 0x6789_abcd},
            ],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x00, 0x00, 0x00, 0x02, // entry count
            0x01, 0x23, 0x45, 0x67, // sample count
            0x23, 0x45, 0x67, 0x89, // sample delta
            0x45, 0x67, 0x89, 0xab, // sample count
            0x67, 0x89, 0xab, 0xcd, // sample delta
        ]; "stts"
    )]
#[test_case(
        Tfdt{
            flags: [0, 0, 0],
            base_media_decode_time: TfdtBaseMediaDecodeTime::V1(0x0123_4567_89ab_cdef)
        },
        &[
            1,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, // base media decode time
        ]; "tfdt: version 1"
    )]
#[test_case(
        Tfhd{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            track_id: 0x0840_4649,
            base_data_offset: 0,
            sample_description_index: 0,
            default_sample_duration: 0,
            default_sample_size: 0,
            default_sample_flags: 0,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x08, 0x40, 0x46, 0x49, // track ID
        ]; "tfhd: no flags"
    )]
#[test_case(
        Tfhd{
            full_box: FullBox{
                version: 0,
                flags: [
                    0,
                    0,
                    (TFHD_BASE_DATA_OFFSET_PRESENT | TFHD_DEFAULT_SAMPLE_DURATION_PRESENT) as u8,
                ],
            },
            track_id: 0x0840_4649,
            base_data_offset: 0x0123_4567_89ab_cdef,
            sample_description_index: 0,
            default_sample_duration: 0x2345_6789,
            default_sample_size: 0,
            default_sample_flags: 0,
        },
        &[
            0,                // version
            0x00, 0x00, 0x09, // flags (0000 0000 1001)
            0x08, 0x40, 0x46, 0x49, // track ID
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x23, 0x45, 0x67, 0x89,
        ]; "tfhd: base data offset & default sample duration"
    )]
#[test_case(
        Tkhd{
            flags: [0, 0, 0],
            version: TkhdVersion::V0(TkhdV0{
                creation_time: 0x0123_4567,
                modification_time: 0x1234_5678,
                duration: 0x4567_89ab,
            }),
            track_id: 0x2345_6789,
            reserved0: 0x3456_789a,
            reserved1: [0, 0],
            layer: 23456,  // 0x5ba0
            alternate_group: -23456, // 0xdba0
            volume: 0x0100,
            reserved2: 0,
            matrix: [
                0x0001_0000, 0, 0,
                0, 0x0001_0000, 0,
                0, 0, 0x4000_0000,
            ],
            width:  125_829_120,
            height: 70_778_880,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, // creation time
            0x12, 0x34, 0x56, 0x78, // modification time
            0x23, 0x45, 0x67, 0x89, // track ID
            0x34, 0x56, 0x78, 0x9a, // reserved
            0x45, 0x67, 0x89, 0xab, // duration
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            0x5b, 0xa0, // layer
            0xa4, 0x60, // alternate group
            0x01, 0x00, // volume
            0x00, 0x00, // reserved
            0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, // matrix
            0x07, 0x80, 0x00, 0x00, // width
            0x04, 0x38, 0x00, 0x00, // height
        ]; "tkhd version 0"
    )]
#[test_case(
        Tkhd{
            flags: [0, 0, 0],
            version: TkhdVersion::V1(TkhdV1{
                creation_time: 0x0123_4567_89ab_cdef,
                modification_time: 0x1234_5678_9abc_def0,
                duration: 0x4567_89ab_cdef_0123,
            }),
            track_id: 0x2345_6789,
            reserved0: 0x3456_789a,
            reserved1: [0, 0],
            layer: 23456,  // 0x5ba0
            alternate_group: -23456, // 0xdba0
            volume: 0x0100,
            reserved2: 0,
            matrix: [
                0x0001_0000, 0, 0,
                0, 0x0001_0000, 0,
                0, 0, 0x4000_0000,
            ],
            width:  125_829_120,
            height: 70_778_880,
        },
        &[
            1,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, // creation time
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, // modification time
            0x23, 0x45, 0x67, 0x89, // track ID
            0x34, 0x56, 0x78, 0x9a, // reserved
            0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, // duration
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
            0x5b, 0xa0, // layer
            0xa4, 0x60, // alternate group
            0x01, 0x00, // volume
            0x00, 0x00, // reserved
            0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, // matrix
            0x07, 0x80, 0x00, 0x00, // width
            0x04, 0x38, 0x00, 0x00, // height
        ]; "tkhd version 1"
    )]
#[test_case(Traf{}, &[]; "traf")]
#[test_case(Trak{}, &[]; "trak")]
#[test_case(
        Trex{
            full_box: FullBox{
                version: 0,
                flags: [0, 0, 0],
            },
            track_id: 0x0123_4567,
            default_sample_description_index: 0x2345_6789,
            default_sample_duration: 0x4567_89ab,
            default_sample_size: 0x6789_abcd,
            default_sample_flags: 0x89ab_cdef,
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, 0x45, 0x67, // track ID
            0x23, 0x45, 0x67, 0x89, // default sample description index
            0x45, 0x67, 0x89, 0xab, // default sample duration
            0x67, 0x89, 0xab, 0xcd, // default sample size
            0x89, 0xab, 0xcd, 0xef, // default sample flags
        ]; "trex"
    )]
#[test_case(
        Trun{
            flags: [0, 8, 0],
            data_offset: 0,
            first_sample_flags: 0,
            entries: TrunEntries::V1(vec![
                TrunEntryV1{
                    sample_duration: 0,
                    sample_size: 0,
                    sample_flags: 0,
                    sample_composition_time_offset: 200,
                },
                TrunEntryV1{
                    sample_duration: 0,
                    sample_size: 0,
                    sample_flags: 0,
                    sample_composition_time_offset: 201,
                },
                TrunEntryV1{
                    sample_duration: 0,
                    sample_size: 0,
                    sample_flags: 0,
                    sample_composition_time_offset: -202,
                },
            ]),
        },
        &[
            1,                // version
            0x00, 0x08, 0x00, // flags
            0x00, 0x00, 0x00, 0x03, // sample count
            0x00, 0x00, 0x00, 0xc8, // sample composition time offset
            0x00, 0x00, 0x00, 0xc9, // sample composition time offset
            0xff, 0xff, 0xff, 0x36, // sample composition time offset
        ]; "trun: version=1 flag=0x800"
    )]
#[test_case(
        Vmhd{
            full_box: FullBox{
                version: 0,
                flags:   [0, 0, 0],
            },
            graphics_mode: 0x0123,
            opcolor:      [0x2345, 0x4567, 0x6789],
        },
        &[
            0,                // version
            0x00, 0x00, 0x00, // flags
            0x01, 0x23, // graphics mode
            0x23, 0x45, 0x45, 0x67, 0x67, 0x89, // opcolor
        ]; "vmhd"
    )]
#[tokio::test]
async fn test_box_types<T: Into<Box<dyn ImmutableBoxBoth>>>(src: T, bin: &[u8]) {
    let src = src.into();
    let size = src.size();

    // Sync.
    let mut buf = Vec::<u8>::with_capacity(size);
    src.marshal_sync(&mut buf).unwrap();
    assert_eq!(size, buf.len());
    assert_eq!(bin, buf);

    // Async.
    let mut buf = Vec::<u8>::with_capacity(size);
    src.marshal_async(&mut buf).await.unwrap();
    assert_eq!({ size }, buf.len());
    assert_eq!(bin, buf);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_generate_init() {
    let extra_data = vec![
        0x1, 0x64, 0x0, 0x16, 0x3, 0x1, 0x0, 0x1b, 0x67, 0x64, 0x0, 0x16, 0xac, 0xd9, 0x40, 0xa4,
        0x3b, 0xe4, 0x88, 0xc0, 0x44, 0x0, 0x0, 0x3, 0x0, 0x4, 0x0, 0x0, 0x3, 0x0, 0x60, 0x3c,
        0x58, 0xb6, 0x58, 0x1, 0x0, 0x0,
    ];

    let got = generate_init(650, 450, extra_data).unwrap();

    let want = vec![
        0, 0, 0, 0x20, b'f', b't', b'y', b'p', //
        b'm', b'p', b'4', b'2', // Major brand.
        0, 0, 0, 1, // Minor version.
        b'm', b'p', b'4', b'1', // Compatible brand.
        b'm', b'p', b'4', b'2', // Compatible brand.
        b'i', b's', b'o', b'm', // Compatible brand.
        b'h', b'l', b's', b'f', // Compatible brand.
        0, 0, 2, 0x89, b'm', b'o', b'o', b'v', //
        0, 0, 0, 0x6c, b'm', b'v', b'h', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Creation time.
        0, 0, 0, 0, // Modification time.
        0, 0, 3, 0xe8, // Time scale.
        0, 0, 0, 0, // Duration.
        0, 1, 0, 0, // Rate.
        1, 0, // Volume.
        0, 0, // Reserved.
        0, 0, 0, 0, 0, 0, 0, 0, // Reserved2.
        0, 1, 0, 0, // 1 Matrix.
        0, 0, 0, 0, // 2.
        0, 0, 0, 0, // 3.
        0, 0, 0, 0, // 4.
        0, 1, 0, 0, // 5.
        0, 0, 0, 0, // 6.
        0, 0, 0, 0, // 7.
        0, 0, 0, 0, // 8.
        0x40, 0, 0, 0, // 9.
        0, 0, 0, 0, // 1 Predefined.
        0, 0, 0, 0, // 2.
        0, 0, 0, 0, // 3.
        0, 0, 0, 0, // 4.
        0, 0, 0, 0, // 5.
        0, 0, 0, 0, // 6.
        0, 0, 0, 2, // Next track ID.
        0, 0, 1, 0xed, b't', b'r', b'a', b'k', // Video.
        0, 0, 0, 0x5c, b't', b'k', b'h', b'd', //
        0, 0, 0, 3, // FullBox.
        0, 0, 0, 0, // Creation time.
        0, 0, 0, 0, // Modification time.
        0, 0, 0, 1, // Track ID.
        0, 0, 0, 0, // Reserved0.
        0, 0, 0, 0, // Duration.
        0, 0, 0, 0, 0, 0, 0, 0, // Reserved1.
        0, 0, // Layer.
        0, 0, // Alternate group.
        0, 0, // Volume.
        0, 0, // Reserved2.
        0, 1, 0, 0, // 1 Matrix.
        0, 0, 0, 0, // 2.
        0, 0, 0, 0, // 3.
        0, 0, 0, 0, // 4.
        0, 1, 0, 0, // 5.
        0, 0, 0, 0, // 6.
        0, 0, 0, 0, // 7.
        0, 0, 0, 0, // 8.
        0x40, 0, 0, 0, // 9.
        2, 0x8a, 0, 0, // Width
        1, 0xc2, 0, 0, // Height
        0, 0, 1, 0x89, b'm', b'd', b'i', b'a', //
        0, 0, 0, 0x20, b'm', b'd', b'h', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Creation time.
        0, 0, 0, 0, // Modification time.
        0, 1, 0x5f, 0x90, // Time scale.
        0, 0, 0, 0, // Duration.
        0x55, 0xc4, // Language.
        0, 0, // Predefined.
        0, 0, 0, 0x2d, b'h', b'd', b'l', b'r', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Predefined.
        b'v', b'i', b'd', b'e', // Handler type.
        0, 0, 0, 0, // Reserved.
        0, 0, 0, 0, //
        0, 0, 0, 0, //
        b'V', b'i', b'd', b'e', b'o', b'H', b'a', b'n', b'd', b'l', b'e', b'r', 0, //
        0, 0, 1, 0x34, b'm', b'i', b'n', b'f', //
        0, 0, 0, 0x14, b'v', b'm', b'h', b'd', //
        0, 0, 0, 1, // FullBox.
        0, 0, // Graphics mode.
        0, 0, 0, 0, 0, 0, // OpColor.
        0, 0, 0, 0x24, b'd', b'i', b'n', b'f', //
        0, 0, 0, 0x1c, b'd', b'r', b'e', b'f', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 1, // Entry count.
        0, 0, 0, 0xc, b'u', b'r', b'l', b' ', //
        0, 0, 0, 1, // FullBox.
        0, 0, 0, 0xf4, b's', b't', b'b', b'l', //
        0, 0, 0, 0xa8, b's', b't', b's', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 1, // Entry count.
        0, 0, 0, 0x98, b'a', b'v', b'c', b'1', //
        0, 0, 0, 0, 0, 0, // Reserved.
        0, 1, // Data reference index.
        0, 0, // Predefined.
        0, 0, // Reserved.
        0, 0, 0, 0, // Predefined2.
        0, 0, 0, 0, 0, 0, 0, 0, 2, 0x8a, // Width.
        1, 0xc2, // Height.
        0, 0x48, 0, 0, // Horizresolution
        0, 0x48, 0, 0, // Vertresolution
        0, 0, 0, 0, // Reserved2.
        0, 1, // Frame count.
        0, 0, 0, 0, 0, 0, 0, 0, // Compressor name.
        0, 0, 0, 0, 0, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, //
        0, 0x18, // Depth.
        0xff, 0xff, // Predefined3.
        0, 0, 0, 0x2e, b'a', b'v', b'c', b'C', //
        1,    // Configuration version.
        0x64, // Profile.
        0,    // Profile compatibility.
        0x16, // Level.
        3,    // Reserved, Length size minus one.
        1,    // Reserved, N sequence parameters.
        0, 0x1b, // Length 27.
        0x67, 0x64, 0, 0x16, 0xac, // Parameter set.
        0xd9, 0x40, 0xa4, 0x3b, 0xe4, //
        0x88, 0xc0, 0x44, 0, 0, //
        3, 0, 4, 0, 0, //
        3, 0, 0x60, 0x3c, 0x58, //
        0xb6, 0x58, //
        1,    // Reserved N sequence parameters.
        0, 0, // Length.
        0, 0, 0, 0x14, b'b', b't', b'r', b't', //
        0, 0, 0, 0, // Buffer size.
        0, 0xf, 0x42, 0x40, // Max bitrate.
        0, 0xf, 0x42, 0x40, // Average bitrate.
        0, 0, 0, 0x10, b's', b't', b't', b's', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Entry count.
        0, 0, 0, 0x10, b's', b't', b's', b'c', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Entry count.
        0, 0, 0, 0x14, b's', b't', b's', b'z', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Sample size.
        0, 0, 0, 0, // Sample count.
        0, 0, 0, 0x10, b's', b't', b'c', b'o', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Entry count.
        0, 0, 0, 0x28, b'm', b'v', b'e', b'x', //
        0, 0, 0, 0x20, b't', b'r', b'e', b'x', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 1, // Track ID.
        0, 0, 0, 1, // Default sample description index.
        0, 0, 0, 0, // Default sample duration.
        0, 0, 0, 0, // Default sample size.
        0, 0, 0, 0, // Default sample flags.
    ];
    assert_eq!(want, got);
}

#[test]
fn test_generate_moof_minimal() {
    let samples = [FragmentSample {
        pts: 0,
        cto: 0,
        duration: 0,
        size: 0,
        non_sync: true,
    }];

    let got = generate_moof(0, &samples).unwrap();

    let want = vec![
        0, 0, 0, 0x68, b'm', b'o', b'o', b'f', //
        0, 0, 0, 0x10, b'm', b'f', b'h', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Sequence number.
        0, 0, 0, 0x50, b't', b'r', b'a', b'f', //
        0, 0, 0, 0x10, b't', b'f', b'h', b'd', //
        0, 2, 0, 0, // Track id.
        0, 0, 0, 1, // Sample size.
        0, 0, 0, 0x14, b't', b'f', b'd', b't', //
        1, 0, 0, 0, // Track id.
        0, 0, 0, 0, 0, 0, 0, 0, // BaseMediaDecodeTime.
        0, 0, 0, 0x24, b't', b'r', b'u', b'n', // Video trun.
        1, 0, 0xf, 1, // FullBox.
        0, 0, 0, 1, // Sample count.
        0, 0, 0, 0x70, // Data offset.
        0, 0, 0, 0, // Entry sample duration.
        0, 0, 0, 0, // Entry sample size.
        0, 1, 0, 0, // Entry sample flags.
        0, 0, 0, 0, // Entry SampleCompositionTimeOffset
    ];
    assert_eq!(want, got);
}

#[test]
fn test_generate_moof_video_sample() {
    let samples = [FragmentSample {
        pts: 0,
        cto: 0,
        duration: 0,
        size: 4,
        non_sync: true,
    }];

    let got = generate_moof(0, &samples).unwrap();

    let want = vec![
        0, 0, 0, 0x68, b'm', b'o', b'o', b'f', //
        0, 0, 0, 0x10, b'm', b'f', b'h', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Sequence number.
        0, 0, 0, 0x50, b't', b'r', b'a', b'f', // Video traf.
        0, 0, 0, 0x10, b't', b'f', b'h', b'd', // Video tfhd.
        0, 2, 0, 0, // Track id.
        0, 0, 0, 1, // Sample size.
        0, 0, 0, 0x14, b't', b'f', b'd', b't', // Video tfdt.
        1, 0, 0, 0, // Track id.
        0, 0, 0, 0, 0, 0, 0, 0, // BaseMediaDecodeTime.
        0, 0, 0, 0x24, b't', b'r', b'u', b'n', // Video trun.
        1, 0, 0xf, 1, // FullBox.
        0, 0, 0, 1, // Sample count.
        0, 0, 0, 0x70, // Data offset.
        0, 0, 0, 0, // Entry sample duration.
        0, 0, 0, 4, // Entry sample size.
        0, 1, 0, 0, // Entry sample flags.
        0, 0, 0, 0, // Entry SampleCompositionTimeOffset
    ];
    assert_eq!(want, got);
}

#[test]
fn test_generate_moof_multiple_video_samples() {
    let samples = [
        FragmentSample {
            pts: 0,
            cto: 0,
            duration: 0,
            size: 4,
            non_sync: false,
        },
        FragmentSample {
            pts: 0,
            cto: 0,
            duration: 0,
            size: 4,
            non_sync: true,
        },
        FragmentSample {
            pts: 0,
            cto: 0,
            duration: 0,
            size: 4,
            non_sync: true,
        },
    ];

    let got = generate_moof(0, &samples).unwrap();

    let want = vec![
        0, 0, 0, 0x88, b'm', b'o', b'o', b'f', //
        0, 0, 0, 0x10, b'm', b'f', b'h', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Sequence number.
        0, 0, 0, 0x70, b't', b'r', b'a', b'f', // Video traf.
        0, 0, 0, 0x10, b't', b'f', b'h', b'd', // Video tfhd.
        0, 2, 0, 0, // Track id.
        0, 0, 0, 1, // Sample size.
        0, 0, 0, 0x14, b't', b'f', b'd', b't', // Video tfdt.
        1, 0, 0, 0, // Track id.
        0, 0, 0, 0, 0, 0, 0, 0, // BaseMediaDecodeTime.
        0, 0, 0, 0x44, b't', b'r', b'u', b'n', // Video trun.
        1, 0, 0xf, 1, // FullBox.
        0, 0, 0, 3, // Sample count.
        0, 0, 0, 0x90, // Data offset.
        0, 0, 0, 0, // Entry1 sample duration.
        0, 0, 0, 4, // Entry1 sample size.
        0, 0, 0, 0, // Entry1 sample flags.
        0, 0, 0, 0, // Entry1 SampleCompositionTimeOffset
        0, 0, 0, 0, // Entry2 sample duration.
        0, 0, 0, 4, // Entry2 sample size.
        0, 1, 0, 0, // Entry2 sample flags.
        0, 0, 0, 0, // Entry2 SampleCompositionTimeOffset
        0, 0, 0, 0, // Entry3 sample duration.
        0, 0, 0, 4, // Entry3 sample size.
        0, 1, 0, 0, // Entry3 sample flags.
        0, 0, 0, 0, // Entry3 SampleCompositionTimeOffset
    ];
    assert_eq!(want, got);
}

#[test]
fn test_generate_moof_minimal_real() {
    // start_time = 1_000_000_000 seconds at 90 kHz.
    let start_time = 1_000_000_000 * 90000;
    let samples = [
        FragmentSample {
            pts: start_time + 54000,
            cto: 54000 - 60000,
            duration: 11999,
            size: 4,
            non_sync: false,
        },
        FragmentSample {
            pts: start_time + 63000,
            cto: 63000 - 72000,
            duration: 9000,
            size: 4,
            non_sync: true,
        },
    ];

    let got = generate_moof(start_time, &samples).unwrap();

    let want = vec![
        0, 0, 0, 0x78, b'm', b'o', b'o', b'f', //
        0, 0, 0, 0x10, b'm', b'f', b'h', b'd', //
        0, 0, 0, 0, // FullBox.
        0, 0, 0, 0, // Sequence number.
        0, 0, 0, 0x60, b't', b'r', b'a', b'f', // Video traf.
        0, 0, 0, 0x10, b't', b'f', b'h', b'd', // Video tfhd.
        0, 2, 0, 0, // Track id.
        0, 0, 0, 1, // Sample size.
        0, 0, 0, 0x14, b't', b'f', b'd', b't', // Video tfdt.
        1, 0, 0, 0, // Track id.
        0, 0, 0, 0, 0, 0, 0xea, 0x60, // BaseMediaDecodeTime.
        0, 0, 0, 0x34, b't', b'r', b'u', b'n', // Video trun.
        1, 0, 0xf, 1, // FullBox.
        0, 0, 0, 2, // Sample count.
        0, 0, 0, 0x80, // Data offset.
        0, 0, 0x2e, 0xdf, // Entry1 sample duration.
        0, 0, 0, 4, // Entry1 sample size.
        0, 0, 0, 0, // Entry1 sample flags.
        0xff, 0xff, 0xe8, 0x90, // 1 Entry SampleCompositionTimeOffset
        0, 0, 0x23, 0x28, // 2 Entry sample duration.
        0, 0, 0, 4, // 2 Entry sample size.
        0, 1, 0, 0, // 2 Entry sample flags.
        0xff, 0xff, 0xdc, 0xd8, // Entry2 SampleCompositionTimeOffset
    ];
    assert_eq!(want, got);
}
