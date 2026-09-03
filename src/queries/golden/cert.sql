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
 WHERE c.id = $1