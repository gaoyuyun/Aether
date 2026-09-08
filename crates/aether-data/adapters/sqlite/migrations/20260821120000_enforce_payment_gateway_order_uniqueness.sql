-- A gateway transaction identifier may repeat across payment methods, but
-- must never identify two orders in the same method. If historical conflicts
-- exist, index creation intentionally fails without modifying financial data.
-- Diagnose with:
-- SELECT payment_method, gateway_order_id, COUNT(*)
-- FROM payment_orders
-- WHERE gateway_order_id IS NOT NULL
-- GROUP BY payment_method, gateway_order_id
-- HAVING COUNT(*) > 1;
UPDATE payment_orders
SET payment_method = lower(trim(payment_method))
WHERE payment_method <> lower(trim(payment_method));

UPDATE payment_callbacks
SET payment_method = lower(trim(payment_method))
WHERE payment_method <> lower(trim(payment_method));

CREATE UNIQUE INDEX uq_payment_orders_payment_method_gateway_order_id
    ON payment_orders (payment_method, gateway_order_id);
