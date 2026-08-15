-- Manual proxy credentials can select different upstream routes through the
-- same host and port. Manual nodes are identified by their UUID. Automatically
-- registered tunnel nodes continue to reuse endpoints in repository logic.
ALTER TABLE public.proxy_nodes
    DROP CONSTRAINT IF EXISTS uq_proxy_node_ip_port;
