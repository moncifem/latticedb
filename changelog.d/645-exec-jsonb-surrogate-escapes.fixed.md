jsonb casts now refuse an unpaired surrogate escape (22P02) and a NUL escape (22P05), as PostgreSQL does; both were accepted before.
