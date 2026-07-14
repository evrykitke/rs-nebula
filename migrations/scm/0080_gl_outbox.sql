-- The GL posting outbox: every SCM posting with a financial effect stages
-- its GlPostingRequested payload here, in the SAME transaction that posts
-- the document — so a crash between commit and publish can never lose the
-- request. The row is deleted when accounting acknowledges the source
-- (gl.posting_booked); a sweeper re-publishes anything that lingers.
CREATE TABLE scm_gl_outbox (
    source          TEXT PRIMARY KEY,          -- "{module}.{doc}:{id}:{action}"
    payload         JSONB NOT NULL,            -- the serialized posting request
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TIMESTAMPTZ
);
CREATE INDEX ix_scm_gl_outbox_created ON scm_gl_outbox (created_at);
