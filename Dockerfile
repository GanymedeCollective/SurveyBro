FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev tzdata
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/survey-bro /survey-bro
RUN adduser -D -H survey-bro

FROM scratch
COPY --from=build /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=build /etc/passwd /etc/group /etc/
COPY --from=build /survey-bro /survey-bro
USER survey-bro
ENTRYPOINT ["/survey-bro"]
