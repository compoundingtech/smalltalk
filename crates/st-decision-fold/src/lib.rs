//! Pure decision record grammar, validation, and state fold.
//! No filesystem storage, CLI, or daemon integration.

mod evaluation;
mod model;
mod parsing;

pub use evaluation::{
    Fold, MAX_GUARD_DEPTH, MAX_GUARD_TERMS, MAX_RECORDS, Resolution, State,
    answer_history, assumptions_for, current_answer, fold, promotions_for,
};
pub use model::{
    Answer, AnswerProvenance, Assumption, DecisionError, Defect, DefectCode, Kind,
    Promotion, Request, Result, Store, Term,
};
pub use parsing::{
    OptionBlock, Record, handle, is_option_key, looks_like_handle, parse_filename,
    parse_options, parse_record, parse_term, validate_request_body,
};

#[cfg(test)]
mod tests;
