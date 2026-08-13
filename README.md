# welund

*Old English* Wēland *(Old Norse* Vǫlundr*, reconstructed Proto-Germanic* \*Wēlandaz*) — the master smith of Germanic legend. Imprisoned by a king who wanted his craft kept for himself alone, and hamstrung to keep him from leaving. He forged himself wings from what was left to him, and flew free.*

**An archival-grade, annotation-native ebook format.**

---

<img width="915" height="971" alt="Badhild_in_Wielands_Schmiede" src="https://github.com/user-attachments/assets/09adf7b2-d45d-4a80-bdd6-aac341f1d9ba" />


## The problem

The book survived the codex, the printing press, and five centuries of paper — and then digital reading quietly took something away from it. A physical book carries the mark of every hand that worked it over: an underlined passage, a name and date in the front cover, a margin note in a parent's handwriting that a child finds years later and reads twice — once for the words, once for the hand that wrote them. That mark *is* the book, as much as the text is.

Digital reading lost the craft. EPUB was built to simulate paper — pages, spines, reflowable text — but it never made room for what a reader leaves behind while working through a text. Every annotation you leave today lives in a proprietary silo, tied to one app, one account, one device, forged shut the moment you make it. The format itself never asked the question that mattered: *what happens to the work a reader puts into a book?*

## The idea

welund answers it structurally. Every book compiles into a single self-contained SQLite database — one file, no external dependencies, the same archival container the Library of Congress recognizes for long-term digital preservation. Text isn't flowed HTML; it's a structured, queryable tree. Annotations — highlights, notes, ink, a reader's own recorded voice pinned to the exact words that moved them — aren't a sidecar file an app maintains on your behalf. They're first-class rows in the same database as the text they're anchored to, addressed precisely enough to survive reflow, font changes, translation, even structural revision of the book itself.

That single design choice changes what a book *can be*. A library becomes queryable — full-text search, cross-references, structure — without an app secretly building its own database behind your back just to offer basic functionality EPUB never provided. And a book becomes something you can actually hand down: a parent's marginalia, still there, still anchored to the right words, in the copy their child inherits — a book worked over, not just read.

The name is deliberate. Welund is the smith who was imprisoned for his craft and made himself the means to escape it — a fitting patron for a format built to free a reader's work from platforms that would rather keep it locked to one device, one app, one account. A book compiled to welund carries the marks of everyone who ever worked on it, unlocked, and passed on.

## Who this is for

welund isn't aimed at replacing the incumbents by force of adoption — it's built for the people the incumbents were never built for:

- **Indie ereader developers** who want a format that hands them structure, search, and annotation storage for free, instead of reimplementing all three, badly, on top of styled HTML.
- **DRM-free publishers and small presses** who want their books to belong to their readers, permanently.
- **Archivists and librarians** who need a format built on a container already trusted for long-term digital preservation.
- **Researchers, language learners, book clubs, and serious annotators** — anyone for whom a book is a conversation with the text, not a one-way read.
- **Anyone who has ever wanted to hand a book to someone they love, thoughts and all.**

## What this is not

This project isn't commercially driven, and it isn't trying to dethrone EPUB by scale. It exists because this problem is real, the fix is possible, and it's worth building well — a format designed the way a craftsperson designs a tool meant to outlast them.

---

*A reference compiler (EPUB → `.welund`) and a reference reader client live in this repository. The specification is a work in progress and contributions are welcome.*
