use orgasmic_core::{HeadingLineEdit, OrgFile, OrgRewriter};

#[test]
fn declared_states_survive_read_and_title_edits() {
    let source = "#+todo: ACTIVE | ARCHIVED\n\n* ACTIVE MEET-ABCDE Original\n:PROPERTIES:\n:ID: MEET-ABCDE\n:END:\nNotes.\n";
    let file = OrgFile::parse(source, "node.org").unwrap();
    let heading = file.find_by_id("MEET-ABCDE").unwrap();
    assert_eq!(heading.todo.as_deref(), Some("ACTIVE"));
    assert_eq!(heading.title, "MEET-ABCDE Original");
    let mut writer = OrgRewriter::new(&file, "node.org");
    writer
        .edit_heading_line(
            "MEET-ABCDE",
            &HeadingLineEdit {
                title: Some("MEET-ABCDE Edited".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let edited = OrgFile::parse(writer.finish(), "node.org").unwrap();
    assert_eq!(edited.headings[0].todo.as_deref(), Some("ACTIVE"));
    assert_eq!(edited.headings[0].title, "MEET-ABCDE Edited");
}

#[test]
fn undeclared_uppercase_words_remain_titles() {
    let file = OrgFile::parse("* ACTIVE ordinary prose\n* PROJECT demo\n", "notes.org").unwrap();
    assert!(file.headings.iter().all(|heading| heading.todo.is_none()));
    assert_eq!(file.headings[0].title, "ACTIVE ordinary prose");
}
