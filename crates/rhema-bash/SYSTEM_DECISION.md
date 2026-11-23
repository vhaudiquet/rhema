You can ask a decision model with the `decide` bash function. A decision model or System One models reads a state and typed questions and answers each with calibrated probabilities in one pass. It generates no text. Three question kinds: `noul` (the probability a yes/no statement is true), `choice` (one of the options you define, with a probability per option), `score` (a position on an ordered rubric you write).

    decide noul "Customer: my order never arrived, I want a refund now" "Does the customer ask for a person to step in?"
    decide choice "Customer: my order never arrived, I want a refund now" "Which team handles this" billing="Payments and refunds" logistics="Shipping and delivery"
    decide score "Checkout has failed for every customer for an hour." "How severe is the impact" "No impact" "Minor" "Major" "Critical"

Decisions like that are cheap and fast. Use it in a script on very specific tasks, like triaging 1000 PRs or issues to know which one to prioritise.