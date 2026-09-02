use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio_postgres::types::Type;

use crate::db::Db;
use crate::output::{OutputRecord, fmt_opt, fmt_ts};
use crate::queries::{column, timestamp};

/// Column index of the multi-valued SAN field within `cells()`.
const SANS_COL: usize = 9;

// ARRAY(SELECT ...) collapses the set-returning x509_altNames into a single
// text[] column, so this stays one row per certificate.
const CERT_SQL: &str = "\
SELECT c.id, c.issuer_ca_id, ca.name AS issuer_name,
       x509_subjectName(c.certificate) AS subject,
       x509_commonName(c.certificate) AS common_name,
       encode(x509_serialNumber(c.certificate), 'hex') AS serial,
       x509_notBefore(c.certificate) AS not_before,
       x509_notAfter(c.certificate) AS not_after,
       encode(digest(c.certificate, 'sha256'), 'hex') AS sha256_fingerprint,
       ARRAY(SELECT x509_altNames(c.certificate)) AS sans
  FROM certificate c
  LEFT JOIN ca ON ca.id = c.issuer_ca_id
 WHERE c.id = $1";

#[derive(Serialize)]
pub struct CertDetail {
    pub id: i64,
    pub issuer_ca_id: Option<i32>,
    pub issuer_name: Option<String>,
    pub subject: Option<String>,
    pub common_name: Option<String>,
    pub serial: Option<String>,
    pub not_before: Option<DateTime<Utc>>,
    pub not_after: Option<DateTime<Utc>>,
    pub sha256_fingerprint: Option<String>,
    pub sans: Vec<String>,
}

impl OutputRecord for CertDetail {
    fn headers() -> &'static [&'static str] {
        &[
            "crt.sh ID",
            "Issuer CA ID",
            "Issuer",
            "Subject",
            "Common Name",
            "Serial",
            "Not Before (UTC)",
            "Not After (UTC)",
            "SHA-256 Fingerprint",
            "SANs",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            fmt_opt(self.issuer_ca_id),
            fmt_opt(self.issuer_name.as_deref()),
            fmt_opt(self.subject.as_deref()),
            fmt_opt(self.common_name.as_deref()),
            fmt_opt(self.serial.as_deref()),
            fmt_ts(self.not_before.as_ref()),
            fmt_ts(self.not_after.as_ref()),
            fmt_opt(self.sha256_fingerprint.as_deref()),
            self.sans.join("; "),
        ]
    }

    fn csv_rows(&self) -> Vec<Vec<String>> {
        if self.sans.is_empty() {
            return vec![self.cells()];
        }
        self.sans
            .iter()
            .map(|san| {
                let mut cells = self.cells();
                cells[SANS_COL] = san.clone();
                cells
            })
            .collect()
    }
}

pub async fn run_cert(db: &Db, id: i64) -> Result<Option<CertDetail>> {
    let rows = db.query(CERT_SQL, &[(&id, Type::INT8)]).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(CertDetail {
        id: column(row, "id")?,
        issuer_ca_id: column(row, "issuer_ca_id")?,
        issuer_name: column(row, "issuer_name")?,
        subject: column(row, "subject")?,
        common_name: column(row, "common_name")?,
        serial: column(row, "serial")?,
        not_before: timestamp(row, "not_before")?,
        not_after: timestamp(row, "not_after")?,
        sha256_fingerprint: column(row, "sha256_fingerprint")?,
        sans: column(row, "sans")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(sans: &[&str]) -> CertDetail {
        CertDetail {
            id: 42,
            issuer_ca_id: Some(9),
            issuer_name: Some("Test CA".to_string()),
            subject: None,
            common_name: Some("example.com".to_string()),
            serial: Some("0a1b".to_string()),
            not_before: None,
            not_after: None,
            sha256_fingerprint: None,
            sans: sans.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn headers_and_cells_agree_in_arity() {
        assert_eq!(CertDetail::headers().len(), detail(&[]).cells().len());
    }

    #[test]
    fn sans_col_points_at_the_sans_column() {
        assert_eq!(CertDetail::headers()[SANS_COL], "SANs");
    }

    #[test]
    fn csv_writes_one_row_per_san() {
        let rows = detail(&["example.com", "www.example.com"]).csv_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][SANS_COL], "example.com");
        assert_eq!(rows[1][SANS_COL], "www.example.com");
    }

    #[test]
    fn a_certificate_without_sans_still_writes_one_row() {
        assert_eq!(detail(&[]).csv_rows().len(), 1);
    }
}
