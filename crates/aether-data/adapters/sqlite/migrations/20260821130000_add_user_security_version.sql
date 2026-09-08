ALTER TABLE users
    ADD COLUMN security_version INTEGER NOT NULL DEFAULT 0;

ALTER TABLE user_sessions
    ADD COLUMN security_version INTEGER NOT NULL DEFAULT 0;
