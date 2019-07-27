## Crawler

The endpoints are `/crawl`, `/domain-urls`, `/domain-count`.

Each endpoint is a POST method accpting JSON:
```
{ "root_url": "https://www.rust-lang.org" }
```

Example session (assuming [`httpie`](https://httpie.org/) is installed):

Terminal the first:
```
cargo test
RUST_LOG=debug cargo run   # or try RUST_LOG=info for less insanity
```

Terminal the second:
```
# start crawl
http :8000/crawl root_url=https://www.rust-lang.org/

# view scraped urls
http :8000/domain-urls root_url=https://www.rust-lang.org/
>  [
>  "http://ael.co.uk/",
>  "http://bitfury.com/",
>  "http://blog.algorithmia.com/2016/05/algorithmia-now-supports-python-3-javascript-rust-ruby/",
>  "http://blog.cambridgeconsultants.com/wireless-product-development/presentation-mozillas-rust-and-why-we-love-it/",
>  "http://blog.honeypot.io/open-sourcing-searchspot/",
>  "http://blog.izs.me/post/30036893703/policy-on-trolling",
>  "http://blog.shiftleft.io/",
>  "http://blog.skylight.io/rust-means-never-having-to-close-a-socket/",
>  ...
>  ]


# view urls count
http :8000/domain-count root_url=https://www.rust-lang.org/

> 593
```

I tried it on "https://www.google.com" and it went absolutely crazy for a few minutes,
then segfaulted - nice! (I checked the memory usage, nothing obviously wrong).
