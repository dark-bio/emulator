.DEFAULT_GOAL := validate

# CloudKit connection failures use EX_NOHOST (68). Other errors fail immediately.
.PHONY: staple validate
staple validate:
	@test -n "$(ARTIFACT)" || { printf '%s\n' 'ARTIFACT is required' >&2; exit 2; }
	@for delay in 0 5 15; do \
		if [ "$$delay" -ne 0 ]; then \
			printf 'Ticket service unavailable; retrying in %s seconds\n' "$$delay" >&2; \
			sleep "$$delay"; \
		fi; \
		status=0; \
		xcrun stapler $@ "$(ARTIFACT)" || status=$$?; \
		if [ "$$status" -ne 68 ]; then exit "$$status"; fi; \
	done; \
	exit "$$status"
