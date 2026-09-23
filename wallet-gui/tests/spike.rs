use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use plaine_wallet_gui::Spike;

#[test]
fn a_headless_harness_can_click_and_read_back() {
    let mut harness = Harness::new_ui_state(|ui, s: &mut Spike| s.ui(ui), Spike { count: 0 });
    harness.get_by_label("count: 0");
    harness.get_by_label("Increment").click();
    harness.run();
    harness.get_by_label("count: 1");
    assert_eq!(harness.state().count, 1);
}
