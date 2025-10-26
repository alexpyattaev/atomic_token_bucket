# Atomic token bucket

A basic rate limiting / traffic shaping element. Validated using shuttle.

The basic primitive is `TokenBucket` which provides ability to limit
rate of certain events, while allowing bursts through.
More advanced `KeyedRateLimiter` allows to rate-limit multiple keyed
items, such as connections.

## Cargo features

* keyed_rate_limiter: enables the keyed rate limiter, adds dep on dashmap
* shuttle-test: needed for shuttle tests

## FAQ

* Is this fast enough? Yes.
* Is this safe? Verified with shuttle.
* Is this lock-free?
  * TokenBucket is lock-free.
  * KeyedRateLimiter uses sharded hashmap when new keys are added, and is otherwise lock-free.

### Benches

`cargo bench`

### Testing with shuttle

cargo test -F shuttle-test shuttle_test_token_bucket_race


## Credits:
 * https://github.com/vadorovsky
 * https://github.com/alexpyattaev
