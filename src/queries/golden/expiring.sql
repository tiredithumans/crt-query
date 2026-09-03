SELECT cai.certificate_id AS id, cai.issuer_ca_id, ca.name AS issuer_name,
       cai.name_value AS matched_identity,
       x509_commonName(cai.certificate) AS common_name,
       encode(x509_serialNumber(cai.certificate), 'hex') AS serial,
       x509_notBefore(cai.certificate) AS not_before,
       x509_notAfter(cai.certificate) AS not_after,
       (now() AT TIME ZONE 'UTC') AS server_now
  FROM certificate_and_identities cai
  LEFT JOIN ca ON ca.id = cai.issuer_ca_id
 WHERE plainto_tsquery('certwatch', $1) @@ identities(cai.certificate)
   AND cai.name_value ILIKE ('%' || $1 || '%') ESCAPE ''
   AND x509_notAfter(cai.certificate)
         <= (now() AT TIME ZONE 'UTC') + make_interval(days => $2)
   AND x509_notAfter(cai.certificate)
         >= (now() AT TIME ZONE 'UTC') - make_interval(days => $3)
 LIMIT $4