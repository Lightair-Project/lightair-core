//! What Lightair will read aloud on stream from viewers' text, and how text is cut into what's
//! said: links, contact details and long numbers never reach the voice.

use lightair_voice::filter::{Refused, check, speakable, viewer_text};
use lightair_voice::{read_wav, sentences, write_wav};

#[test]
fn a_viewers_link_is_never_read_aloud() {
    let said = viewer_text("check out https://scam.example/free-subs now", false, 200).unwrap();
    assert!(!said.contains("scam") && !said.contains("http"), "{said}");
}

#[test]
fn a_bare_domain_is_never_read_aloud() {
    let said = viewer_text("go to freesubs.xyz for prizes", false, 200).unwrap();
    assert!(!said.contains("freesubs"), "{said}");
}

#[test]
fn a_message_with_an_email_address_isnt_read_at_all() {
    assert_eq!(viewer_text("mail me at someone@example.com", false, 200), None);
    assert_eq!(check("mail me at someone@example.com", false), Err(Refused::PersonalData));
}

#[test]
fn a_phone_number_isnt_read() {
    assert_eq!(check("call +44 20 7946 0958 tonight", false), Err(Refused::PersonalData));
}

#[test]
fn a_long_number_isnt_read() {
    assert!(viewer_text("my card is 4111111111111111", false, 200).is_none());
}

#[test]
fn a_run_of_one_character_is_refused_as_spam() {
    assert_eq!(check("lolllllllllllllll", false), Err(Refused::Spam));
}

#[test]
fn swearing_is_refused_only_when_family_friendly() {
    assert!(check("what the fuck", false).is_ok());
    assert_eq!(check("what the fuck", true), Err(Refused::Profanity));
}

#[test]
fn a_long_message_is_cut_at_a_word() {
    let said = viewer_text("thanks for the stream it has been amazing all night long", false, 20).unwrap();
    assert!(said.chars().count() <= 20, "{said}");
    assert!("thanks for the stream it has been amazing all night long".starts_with(&said), "cut mid-word: {said}");
}

#[test]
fn markdown_and_links_from_the_model_are_made_sayable() {
    assert_eq!(speakable("**Hi** see [the docs](https://x.example) or https://y.example"), "Hi see the docs or a link");
}

#[test]
fn text_is_said_sentence_by_sentence() {
    assert_eq!(
        sentences("Hello there. How are you? Fine!\nNew line"),
        ["Hello there.", "How are you?", "Fine!", "New line"]
    );
}

#[test]
fn a_decimal_point_doesnt_end_a_sentence() {
    assert_eq!(sentences("It costs 3.50 today. Ok."), ["It costs 3.50 today.", "Ok."]);
}

#[test]
fn speech_written_as_wav_reads_back_the_same() {
    let pcm: Vec<i16> = (0..480).map(|i| ((i as f32 / 10.0).sin() * 12_000.0) as i16).collect();
    assert_eq!(read_wav(&write_wav(&pcm, 24_000)).unwrap(), (pcm, 24_000));
}
