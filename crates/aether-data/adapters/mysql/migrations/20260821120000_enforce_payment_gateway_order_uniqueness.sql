-- A gateway transaction identifier may repeat across payment methods, but
-- must never identify two orders in the same normalized method. MySQL commits
-- persistent DDL implicitly, so reject historical conflicts before any
-- persistent UPDATE or ALTER TABLE. CREATE/DROP TEMPORARY TABLE do not cause an
-- implicit commit, and the leading DROP also makes a same-session retry safe.
-- Diagnose with:
-- SELECT LOWER(TRIM(payment_method)),
--        CONVERT(gateway_order_id USING utf8mb4) COLLATE utf8mb4_0900_bin,
--        COUNT(*)
-- FROM payment_orders
-- WHERE gateway_order_id IS NOT NULL
-- GROUP BY LOWER(TRIM(payment_method)),
--          CONVERT(gateway_order_id USING utf8mb4) COLLATE utf8mb4_0900_bin
-- HAVING COUNT(*) > 1;
DROP TEMPORARY TABLE IF EXISTS aether_payment_gateway_order_uniqueness_preflight;

CREATE TEMPORARY TABLE aether_payment_gateway_order_uniqueness_preflight (
    conflict_marker TINYINT NOT NULL PRIMARY KEY
);

INSERT INTO aether_payment_gateway_order_uniqueness_preflight (conflict_marker)
VALUES (1);

-- Inserting the same marker fails on the first conflicting group. The grouping
-- mirrors the values and collations used by the normalization and final index:
-- payment methods use their existing column collation after LOWER/TRIM, while
-- opaque gateway identifiers use MySQL 8's case-sensitive binary collation.
INSERT INTO aether_payment_gateway_order_uniqueness_preflight (conflict_marker)
SELECT 1
FROM payment_orders
WHERE gateway_order_id IS NOT NULL
GROUP BY
    LOWER(TRIM(payment_method)),
    CONVERT(gateway_order_id USING utf8mb4) COLLATE utf8mb4_0900_bin
HAVING COUNT(*) > 1
LIMIT 1;

DROP TEMPORARY TABLE aether_payment_gateway_order_uniqueness_preflight;

UPDATE payment_orders
SET payment_method = LOWER(TRIM(payment_method))
WHERE BINARY payment_method <> BINARY LOWER(TRIM(payment_method));

UPDATE payment_callbacks
SET payment_method = LOWER(TRIM(payment_method))
WHERE BINARY payment_method <> BINARY LOWER(TRIM(payment_method));

-- Gateway identifiers are opaque and case-sensitive. Changing the column
-- collation and adding the unique index in one ALTER avoids a persistent
-- intermediate schema if either operation fails.
ALTER TABLE payment_orders
    MODIFY COLUMN gateway_order_id VARCHAR(128)
        CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NULL,
    ADD UNIQUE INDEX uq_payment_orders_payment_method_gateway_order_id
        (payment_method, gateway_order_id);
