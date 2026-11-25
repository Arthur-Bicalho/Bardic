//! Integration tests for `HttpSpotify` against a wiremock server.
use bardic_core::model::RepeatMode;
use bardic_core::spotify::{HttpSpotify, PlayRequest, SpotifyApi};
use bardic_core::BardicError;
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "BQD-test-token";

async fn setup() -> (MockServer, HttpSpotify) {
    let server = MockServer::start().await;
    let api = HttpSpotify::with_base_url(format!("{}/v1", server.uri()));
    (server, api)
}

fn no_body(req: &Request) -> bool {
    req.body.is_empty() && req.headers.get("content-length").is_some_and(|v| v.to_str().ok() == Some("0"))
}

// ---------------------------------------------------------------- control calls

#[tokio::test]
async fn play_uris_sends_body_device_and_bearer() {
    let (server, api) = setup().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/play"))
        .and(query_param("device_id", "dev123"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(body_json(json!({
            "uris": ["spotify:track:aaa", "spotify:track:bbb"],
            "offset": {"position": 1},
            "position_ms": 4500
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let req = PlayRequest::Uris {
        uris: vec!["spotify:track:aaa".into(), "spotify:track:bbb".into()],
        offset_index: 1,
        position_ms: 4500,
    };
    api.play(TOKEN, Some("dev123"), &req).await.unwrap();
}

#[tokio::test]
async fn play_context_sends_context_uri_without_device() {
    let (server, api) = setup().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/play"))
        .and(body_json(json!({"context_uri": "spotify:playlist:37i9dQZF1DX", "position_ms": 0})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    // Without a device the query string must not contain device_id.
    let req = PlayRequest::Context { context_uri: "spotify:playlist:37i9dQZF1DX".into(), position_ms: 0 };
    api.play(TOKEN, None, &req).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert!(received[0].url.query().is_none(), "unexpected query: {:?}", received[0].url.query());
}

#[tokio::test]
async fn resume_sends_no_json_body_and_zero_content_length() {
    let (server, api) = setup().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/play"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    api.resume(TOKEN, Some("dev1")).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert!(no_body(&received[0]), "headers: {:?}", received[0].headers);
    assert!(received[0].headers.get("content-type").is_none());
}

#[tokio::test]
async fn pause_next_previous_seek_use_right_verbs_and_empty_bodies() {
    let (server, api) = setup().await;
    for (m, p) in [("PUT", "pause"), ("POST", "next"), ("POST", "previous"), ("PUT", "seek")] {
        Mock::given(method(m))
            .and(path(format!("/v1/me/player/{p}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
    }
    api.pause(TOKEN, None).await.unwrap();
    api.next(TOKEN, None).await.unwrap();
    api.previous(TOKEN, None).await.unwrap();
    api.seek(TOKEN, Some("d"), 12_345).await.unwrap();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 4);
    for r in &received {
        assert!(no_body(r), "{} {} should send Content-Length: 0", r.method, r.url.path());
    }
    let seek = received.iter().find(|r| r.url.path().ends_with("/seek")).unwrap();
    let q: Vec<_> = seek.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    assert!(q.contains(&("position_ms".into(), "12345".into())));
    assert!(q.contains(&("device_id".into(), "d".into())));
}

#[tokio::test]
async fn repeat_shuffle_volume_query_params() {
    let (server, api) = setup().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/repeat"))
        .and(query_param("state", "track"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/repeat"))
        .and(query_param("state", "context"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/shuffle"))
        .and(query_param("state", "true"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "42"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "100"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    api.set_repeat(TOKEN, Some("dev1"), RepeatMode::Track).await.unwrap();
    api.set_repeat(TOKEN, None, RepeatMode::Context).await.unwrap();
    api.set_shuffle(TOKEN, None, true).await.unwrap();
    api.set_volume(TOKEN, None, 42).await.unwrap();
    api.set_volume(TOKEN, None, 250).await.unwrap(); // clamped to 100
}

#[tokio::test]
async fn transfer_sends_device_ids_and_play_flag() {
    let (server, api) = setup().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"device_ids": ["abc"], "play": true})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    api.transfer(TOKEN, "abc", true).await.unwrap();
}

// ---------------------------------------------------------------- playback_state

fn full_playback_json() -> serde_json::Value {
    json!({
      "device": {
        "id": "74ASZWbe4lXaubB36ztrGX",
        "is_active": true,
        "is_private_session": false,
        "is_restricted": false,
        "name": "Kitchen Echo",
        "supports_volume": true,
        "type": "Speaker",
        "volume_percent": 55
      },
      "shuffle_state": true,
      "smart_shuffle": false,
      "repeat_state": "track",
      "timestamp": 1_732_550_000_123u64,
      "context": {
        "external_urls": {"spotify": "https://open.spotify.com/playlist/37i9dQZF1DX"},
        "href": "https://api.spotify.com/v1/playlists/37i9dQZF1DX",
        "type": "playlist",
        "uri": "spotify:playlist:37i9dQZF1DX"
      },
      "progress_ms": 61_234,
      "item": {
        "album": {
          "album_type": "album",
          "artists": [{"id": "1", "name": "Wardruna", "type": "artist", "uri": "spotify:artist:1"}],
          "images": [
            {"height": 64, "url": "https://i.scdn.co/image/small", "width": 64},
            {"height": 640, "url": "https://i.scdn.co/image/large", "width": 640},
            {"height": 300, "url": "https://i.scdn.co/image/medium", "width": 300}
          ],
          "name": "Runaljod - Yggdrasil",
          "uri": "spotify:album:2"
        },
        "artists": [
          {"id": "1", "name": "Wardruna", "type": "artist", "uri": "spotify:artist:1"},
          {"id": "9", "name": "Einar Selvik", "type": "artist", "uri": "spotify:artist:9"}
        ],
        "disc_number": 1,
        "duration_ms": 245_000,
        "explicit": false,
        "is_local": false,
        "name": "Helvegen",
        "popularity": 61,
        "preview_url": null,
        "track_number": 3,
        "type": "track",
        "uri": "spotify:track:5Zk7"
      },
      "currently_playing_type": "track",
      "actions": {"disallows": {"resuming": true}},
      "is_playing": true
    })
}

#[tokio::test]
async fn playback_state_maps_full_payload() {
    let (server, api) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .and(query_param("additional_types", "track,episode"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(full_playback_json()))
        .expect(1)
        .mount(&server)
        .await;

    let st = api.playback_state(TOKEN).await.unwrap().expect("state");
    assert!(st.is_playing);
    let dev = st.device.expect("device");
    assert_eq!(dev.id, "74ASZWbe4lXaubB36ztrGX");
    assert_eq!(dev.name, "Kitchen Echo");
    assert_eq!(dev.device_type, "Speaker");
    assert!(dev.is_active && !dev.is_restricted && dev.supports_volume);
    assert_eq!(dev.volume_percent, Some(55));
    assert_eq!(st.track_uri.as_deref(), Some("spotify:track:5Zk7"));
    assert_eq!(st.track_name.as_deref(), Some("Helvegen"));
    assert_eq!(st.artist_name.as_deref(), Some("Wardruna"));
    assert_eq!(st.album_art_url.as_deref(), Some("https://i.scdn.co/image/large"));
    assert_eq!(st.context_uri.as_deref(), Some("spotify:playlist:37i9dQZF1DX"));
    assert_eq!(st.progress_ms, 61_234);
    assert_eq!(st.duration_ms, 245_000);
    assert_eq!(st.repeat, RepeatMode::Track);
    assert!(st.shuffle);
    assert_eq!(st.fetched_at_ms, 0);
    assert_eq!(st.active_cue_id, None);
    assert_eq!(st.active_environment_id, None);
}

#[tokio::test]
async fn playback_state_204_is_none() {
    let (server, api) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    assert_eq!(api.playback_state(TOKEN).await.unwrap(), None);
}

#[tokio::test]
async fn playback_state_200_empty_body_is_none() {
    let (server, api) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    assert_eq!(api.playback_state(TOKEN).await.unwrap(), None);
}

#[tokio::test]
async fn playback_state_null_item_and_nulls_in_device() {
    // During ads / private sessions Spotify returns item: null, context: null and a device
    // whose volume_percent is null (e.g. some Connect speakers / TVs).
    let (server, api) = setup().await;
    let body = json!({
      "device": {
        "id": "tv1", "is_active": true, "is_private_session": true, "is_restricted": false,
        "name": "Living Room TV", "type": "TV", "volume_percent": null
      },
      "shuffle_state": false,
      "repeat_state": "off",
      "timestamp": 1_732_550_000_999u64,
      "context": null,
      "progress_ms": null,
      "item": null,
      "currently_playing_type": "ad",
      "actions": {"disallows": {"pausing": true}},
      "is_playing": true
    });
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let st = api.playback_state(TOKEN).await.unwrap().expect("state");
    assert!(st.is_playing);
    let dev = st.device.unwrap();
    assert_eq!(dev.volume_percent, None);
    assert!(dev.supports_volume, "supports_volume defaults to true when absent");
    assert_eq!(st.track_uri, None);
    assert_eq!(st.track_name, None);
    assert_eq!(st.artist_name, None);
    assert_eq!(st.album_art_url, None);
    assert_eq!(st.context_uri, None);
    assert_eq!(st.progress_ms, 0);
    assert_eq!(st.duration_ms, 0);
    assert_eq!(st.repeat, RepeatMode::Off);
}

#[tokio::test]
async fn playback_state_episode_uses_show_name_and_art() {
    let (server, api) = setup().await;
    let body = json!({
      "device": {"id": "d", "is_active": true, "is_restricted": false, "name": "Phone",
                 "type": "Smartphone", "volume_percent": 30, "supports_volume": false},
      "shuffle_state": false,
      "repeat_state": "context",
      "context": {"uri": "spotify:show:xyz", "type": "show"},
      "progress_ms": 1000,
      "item": {
        "name": "Episode 12: The Lich King",
        "uri": "spotify:episode:e12",
        "duration_ms": 3_600_000,
        "type": "episode",
        "images": [{"height": 640, "width": 640, "url": "https://i.scdn.co/image/ep"}],
        "show": {"name": "Critical Rolls", "publisher": "Someone",
                 "images": [{"height": 300, "width": 300, "url": "https://i.scdn.co/image/show"}]}
      },
      "currently_playing_type": "episode",
      "is_playing": false
    });
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let st = api.playback_state(TOKEN).await.unwrap().unwrap();
    assert!(!st.is_playing);
    assert_eq!(st.artist_name.as_deref(), Some("Critical Rolls"));
    assert_eq!(st.album_art_url.as_deref(), Some("https://i.scdn.co/image/ep"));
    assert_eq!(st.repeat, RepeatMode::Context);
    assert!(!st.device.unwrap().supports_volume);
}

// ---------------------------------------------------------------- devices

#[tokio::test]
async fn devices_skips_null_ids() {
    let (server, api) = setup().await;
    let body = json!({"devices": [
      {"id": "74ASZWbe4lXaubB36ztrGX", "is_active": false, "is_private_session": false,
       "is_restricted": false, "name": "Pixel Tablet", "supports_volume": true,
       "type": "Tablet", "volume_percent": 80},
      {"id": null, "is_active": false, "is_private_session": false, "is_restricted": true,
       "name": "Web Player (Restricted)", "supports_volume": false, "type": "Computer",
       "volume_percent": null},
      {"id": "bb99", "is_active": true, "is_restricted": false, "name": "DESKTOP-GM",
       "type": "Computer", "volume_percent": null}
    ]});
    Mock::given(method("GET"))
        .and(path("/v1/me/player/devices"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let devs = api.devices(TOKEN).await.unwrap();
    assert_eq!(devs.len(), 2);
    assert_eq!(devs[0].id, "74ASZWbe4lXaubB36ztrGX");
    assert_eq!(devs[0].device_type, "Tablet");
    assert_eq!(devs[0].volume_percent, Some(80));
    assert!(!devs[0].is_active);
    assert_eq!(devs[1].id, "bb99");
    assert!(devs[1].is_active);
    assert_eq!(devs[1].volume_percent, None);
    assert!(devs[1].supports_volume);
}

#[tokio::test]
async fn devices_empty_list() {
    let (server, api) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"devices": []})))
        .mount(&server)
        .await;
    assert!(api.devices(TOKEN).await.unwrap().is_empty());
}

// ---------------------------------------------------------------- search

#[tokio::test]
async fn search_maps_results_and_skips_null_playlists() {
    let (server, api) = setup().await;
    let body = json!({
      "tracks": {
        "href": "https://api.spotify.com/v1/search?q=tavern", "limit": 2, "total": 2,
        "items": [
          {
            "album": {"name": "Tavern Songs", "images": [
              {"height": 300, "url": "https://i.scdn.co/image/t300", "width": 300},
              {"height": 640, "url": "https://i.scdn.co/image/t640", "width": 640},
              {"height": 64, "url": "https://i.scdn.co/image/t64", "width": 64}
            ]},
            "artists": [{"name": "The Bards"}, {"name": "Lute & Co"}],
            "duration_ms": 187_000, "name": "Drunken Dwarf", "uri": "spotify:track:t1"
          },
          {
            "album": {"name": "Single", "images": []},
            "artists": [{"name": "Solo"}],
            "duration_ms": 90_500, "name": "No Art", "uri": "spotify:track:t2"
          }
        ]
      },
      "playlists": {
        "href": "https://api.spotify.com/v1/search?q=tavern", "limit": 3, "total": 300,
        "items": [
          null,
          {"name": "Fantasy Tavern", "uri": "spotify:playlist:p1",
           "owner": {"display_name": "Bard Curator", "id": "curator1"},
           "images": [{"url": "https://i.scdn.co/image/p1", "height": null, "width": null}]},
          null,
          {"name": "Inn Ambience", "uri": "spotify:playlist:p2",
           "owner": {"display_name": null, "id": "spotify"}, "images": []}
        ]
      },
      "albums": {
        "href": "https://api.spotify.com/v1/search?q=tavern", "limit": 1, "total": 40,
        "items": [
          {"name": "Tavern Songs", "uri": "spotify:album:a1", "album_type": "album",
           "artists": [{"name": "The Bards"}, {"name": "Lute & Co"}],
           "images": [{"height": 64, "url": "https://i.scdn.co/image/a64", "width": 64},
                      {"height": 640, "url": "https://i.scdn.co/image/a640", "width": 640}]}
        ]
      }
    });
    Mock::given(method("GET"))
        .and(path("/v1/search"))
        .and(query_param("q", "tavern & inn: \"brawl\""))
        .and(query_param("type", "track,playlist,album"))
        .and(query_param("limit", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;

    // limit 200 is clamped to 50; the query is properly URL-encoded by the client.
    let r = api.search(TOKEN, "tavern & inn: \"brawl\"", 200).await.unwrap();

    assert_eq!(r.tracks.len(), 2);
    assert_eq!(r.tracks[0].spotify_uri, "spotify:track:t1");
    assert_eq!(r.tracks[0].title, "Drunken Dwarf");
    assert_eq!(r.tracks[0].subtitle, "The Bards, Lute & Co");
    assert_eq!(r.tracks[0].art_url.as_deref(), Some("https://i.scdn.co/image/t640"));
    assert_eq!(r.tracks[0].duration_ms, Some(187_000));
    assert_eq!(r.tracks[0].start_offset_ms, 0);
    assert_eq!(r.tracks[1].art_url, None);
    assert_eq!(r.tracks[1].duration_ms, Some(90_500));

    assert_eq!(r.playlists.len(), 2, "null entries skipped");
    assert_eq!(r.playlists[0].spotify_uri, "spotify:playlist:p1");
    assert_eq!(r.playlists[0].title, "Fantasy Tavern");
    assert_eq!(r.playlists[0].subtitle, "Bard Curator");
    assert_eq!(r.playlists[0].art_url.as_deref(), Some("https://i.scdn.co/image/p1"));
    assert_eq!(r.playlists[0].duration_ms, None);
    assert_eq!(r.playlists[1].subtitle, "spotify", "falls back to owner id");
    assert_eq!(r.playlists[1].art_url, None);

    assert_eq!(r.albums.len(), 1);
    assert_eq!(r.albums[0].spotify_uri, "spotify:album:a1");
    assert_eq!(r.albums[0].subtitle, "The Bards, Lute & Co");
    assert_eq!(r.albums[0].art_url.as_deref(), Some("https://i.scdn.co/image/a640"));
    assert_eq!(r.albums[0].duration_ms, None);
}

#[tokio::test]
async fn search_clamps_limit_low_and_tolerates_missing_sections() {
    let (server, api) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v1/search"))
        .and(query_param("limit", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tracks": {"items": []}})))
        .expect(1)
        .mount(&server)
        .await;
    let r = api.search(TOKEN, "x", 0).await.unwrap();
    assert!(r.tracks.is_empty() && r.playlists.is_empty() && r.albums.is_empty());
}

// ---------------------------------------------------------------- error mapping

async fn error_from(template: ResponseTemplate) -> BardicError {
    let (server, api) = setup().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/pause"))
        .respond_with(template)
        .mount(&server)
        .await;
    api.pause(TOKEN, None).await.unwrap_err()
}

#[tokio::test]
async fn error_401_is_not_authenticated() {
    let body = json!({"error": {"status": 401, "message": "The access token expired"}});
    let e = error_from(ResponseTemplate::new(401).set_body_json(body)).await;
    assert!(matches!(e, BardicError::NotAuthenticated), "{e:?}");
}

#[tokio::test]
async fn error_403_is_forbidden_with_message() {
    let body = json!({"error": {"status": 403, "message": "Player command failed: Premium required",
                                "reason": "PREMIUM_REQUIRED"}});
    let e = error_from(ResponseTemplate::new(403).set_body_json(body)).await;
    match e {
        BardicError::Forbidden(m) => assert!(m.contains("Premium required"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn error_404_no_active_device() {
    let body = json!({"error": {"status": 404, "message": "Player command failed: No active device found",
                                "reason": "NO_ACTIVE_DEVICE"}});
    let e = error_from(ResponseTemplate::new(404).set_body_json(body)).await;
    assert!(matches!(e, BardicError::NoActiveDevice), "{e:?}");
}

#[tokio::test]
async fn error_404_on_player_endpoint_without_body_is_no_active_device() {
    let e = error_from(ResponseTemplate::new(404)).await;
    assert!(matches!(e, BardicError::NoActiveDevice), "{e:?}");
}

#[tokio::test]
async fn error_404_on_non_player_endpoint_is_api_error() {
    let (server, api) = setup().await;
    Mock::given(method("GET"))
        .and(path("/v1/search"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": {"status": 404, "message": "Not found"}})),
        )
        .mount(&server)
        .await;
    let e = api.search(TOKEN, "x", 5).await.unwrap_err();
    assert!(matches!(e, BardicError::Api { status: 404, .. }), "{e:?}");
}

#[tokio::test]
async fn error_429_uses_retry_after_header() {
    let e = error_from(ResponseTemplate::new(429).insert_header("Retry-After", "7")).await;
    assert!(matches!(e, BardicError::RateLimited { retry_after_secs: 7 }), "{e:?}");
}

#[tokio::test]
async fn error_429_defaults_to_one_second() {
    let e = error_from(ResponseTemplate::new(429)).await;
    assert!(matches!(e, BardicError::RateLimited { retry_after_secs: 1 }), "{e:?}");
}

#[tokio::test]
async fn error_500_parses_spotify_error_json() {
    let body = json!({"error": {"status": 500, "message": "Server error"}});
    let e = error_from(ResponseTemplate::new(500).set_body_json(body)).await;
    match e {
        BardicError::Api { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "Server error");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn error_502_non_json_body_still_maps_to_api() {
    let e = error_from(ResponseTemplate::new(502).set_body_string("Bad Gateway")).await;
    match e {
        BardicError::Api { status, message } => {
            assert_eq!(status, 502);
            assert_eq!(message, "Bad Gateway");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn transport_error_is_network() {
    // Nothing listens on this port.
    let api = HttpSpotify::with_base_url("http://127.0.0.1:1/v1");
    let e = api.devices(TOKEN).await.unwrap_err();
    assert!(matches!(e, BardicError::Network(_)), "{e:?}");
}
