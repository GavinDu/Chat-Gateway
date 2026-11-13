These generated RSA fixtures are public test data, never production credentials.
The private key signs synthetic identity tokens only in unit tests; the matching
JWK is injected into the verifier. Production obtains keys from OpenAI's verified
HTTPS discovery document and cannot load these fixtures.
