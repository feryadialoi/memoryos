-- The audit log is append-only for the application. 0001 granted the app role UPDATE and DELETE on
-- every tenant table, audit_events included, so a compromised or buggy server could rewrite or erase
-- the record of what it did. The server only ever inserts and reads audit rows.
REVOKE UPDATE, DELETE ON audit_events FROM memoryos_app;
