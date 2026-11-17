//! 결과 열 타입 이름. pgx 기본 타입 표(pgtype_default.go)에 있는 OID 만 이름으로 보이고, 나머지(사용자 정의 타입·timetz·money 등)는
//! Go 판처럼 OID 숫자 그대로 싣는다.

/// pgx 가 기본으로 아는 (OID, 이름)이다. OID 순서라 이진 탐색한다.
#[rustfmt::skip]
const PGX_TYPES: &[(u32, &str)] = &[
    (16, "bool"), (17, "bytea"), (18, "char"), (19, "name"), (20, "int8"), (21, "int2"), (23, "int4"), (25, "text"), (26, "oid"),
    (27, "tid"), (28, "xid"), (29, "cid"), (114, "json"), (142, "xml"), (143, "_xml"), (199, "_json"), (271, "_xid8"), (600, "point"),
    (601, "lseg"), (602, "path"), (603, "box"), (604, "polygon"), (628, "line"), (629, "_line"), (650, "cidr"), (651, "_cidr"),
    (700, "float4"), (701, "float8"), (705, "unknown"), (718, "circle"), (719, "_circle"), (774, "macaddr8"), (829, "macaddr"),
    (869, "inet"), (1000, "_bool"), (1001, "_bytea"), (1002, "_char"), (1003, "_name"), (1005, "_int2"), (1007, "_int4"),
    (1009, "_text"), (1010, "_tid"), (1011, "_xid"), (1012, "_cid"), (1014, "_bpchar"), (1015, "_varchar"), (1016, "_int8"),
    (1017, "_point"), (1018, "_lseg"), (1019, "_path"), (1020, "_box"), (1021, "_float4"), (1022, "_float8"), (1027, "_polygon"),
    (1028, "_oid"), (1033, "aclitem"), (1034, "_aclitem"), (1040, "_macaddr"), (1041, "_inet"), (1042, "bpchar"), (1043, "varchar"),
    (1082, "date"), (1083, "time"), (1114, "timestamp"), (1115, "_timestamp"), (1182, "_date"), (1183, "_time"), (1184, "timestamptz"),
    (1185, "_timestamptz"), (1186, "interval"), (1187, "_interval"), (1231, "_numeric"), (1560, "bit"), (1561, "_bit"),
    (1562, "varbit"), (1563, "_varbit"), (1700, "numeric"), (2249, "record"), (2287, "_record"), (2950, "uuid"), (2951, "_uuid"),
    (3802, "jsonb"), (3807, "_jsonb"), (3904, "int4range"), (3905, "_int4range"), (3906, "numrange"), (3907, "_numrange"),
    (3908, "tsrange"), (3909, "_tsrange"), (3910, "tstzrange"), (3911, "_tstzrange"), (3912, "daterange"), (3913, "_daterange"),
    (3926, "int8range"), (3927, "_int8range"), (4072, "jsonpath"), (4073, "_jsonpath"), (4451, "int4multirange"),
    (4532, "nummultirange"), (4533, "tsmultirange"), (4534, "tstzmultirange"), (4535, "datemultirange"), (4536, "int8multirange"),
    (5069, "xid8"),
];

/// 열 타입 OID 의 이름이다. pgx 가 모르는 OID 는 숫자다.
pub(crate) fn type_name(oid: u32) -> String {
    match PGX_TYPES.binary_search_by_key(&oid, |&(o, _)| o) {
        Ok(i) => PGX_TYPES[i].1.to_string(),
        Err(_) => oid.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgx_가_아는_타입은_이름이고_모르면_OID_다() {
        assert!(PGX_TYPES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(PGX_TYPES.len(), 104);
        assert_eq!(type_name(23), "int4");
        assert_eq!(type_name(1009), "_text");
        assert_eq!(type_name(1266), "1266"); // timetz
    }
}
