//! This module models the APS3200 APU.
//!
//! Internally it contains a state machine with the following states:
//! - Shutdown
//! - Starting
//! - Running
//! - Stopping
//!
//! > Not all characteristics have been verified as of yet. Meaning things such as
//! > EGT increases, EGT warning level, etc. are there but might not reflect the
//! > real APU fully. This involves further tweaking as we get more information.
//!
//! # Remaining work and questions
//! - What does "the APU speed is always 100% except for air conditioning, ..." mean?
//!   Komp vaguely remembers this as "when APU bleed is supplying the packs, the rpm reduces to 99%".
//! - When the aircraft has ground power or main gen power, the APU page appears on the ECAM.
//!   At this time we have no ECAM "controller" within the system software, and thus we cannot model
//!   this. We probably want to have some event for this.
//! - As above, the APU page disappears on the ECAM 10 seconds after AVAIL came on.
//! - Automatic shutdown:
//!   - Flap not open.
//!   - EGT overtemperature.
//!   - DC Power Loss (BAT OFF when aircraft on batteries only).
//!   - There are more situations, but we likely won't model all of them.
//! - What happens when you abort the start sequence of the APU? Can you? Komp:
//!   I can't find any reference from that, but I assume the APU will finish its start
//!   sequence and then turn off immediately. It is never a good idea to interrupt the
//!   start unless there is some kind of danger. When unburned fuel remains in the
//!   combustion section, it will ignite at the next APU start and shoot a flame out
//!   out the exhaust
//! - What if during the APU cool down the MASTER SW is pushed back ON?
//!   Komp: I'm pretty sure this will cancel the shutdown and the APU will continue like it never happened.
//! - Effect of APU fire pb on APU state.
//! - EGT MAX improvements: "is a function of N during start and a function of ambient
//!   temperature when running".
//! - Advanced electrical scenarios:
//!   - ECB and starter motor is supplied by DC BAT.
//!   - When in electrical emergency config, battery contactors close for max 3 mins when
//!     APU MASTER SW is on.
//!   - When in flight, and in electrical emergency config, APU start is inhibited for 45 secs.

use std::time::Duration;

use rand::prelude::*;
use uom::si::{f64::*, ratio::percent, thermodynamic_temperature::degree_celsius};

use crate::{
    overhead::OnOffPushButton,
    pneumatic::{BleedAirValve, PneumaticOverheadPanel},
    shared::UpdateContext,
    visitor::Visitable,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum ShutdownReason {
    Manual,
    Automatic, // Will be split further later into all kinds of reasons for automatic shutdown.
}

pub struct AuxiliaryPowerUnit {
    state: Option<Box<dyn ApuState>>,
    egt_warning_temp: ThermodynamicTemperature,
}
impl AuxiliaryPowerUnit {
    // TODO: Is this maximum correct for the Honeywell 131-9A?
    // Manual says max EGT is 1090 degree celsius during start and 675 degree celsius while running.
    // That might be for a different model.
    const WARNING_MAX_TEMPERATURE: f64 = 1200.;

    pub fn new() -> AuxiliaryPowerUnit {
        AuxiliaryPowerUnit {
            state: Some(Box::new(Shutdown::new(
                AirIntakeFlap::new(),
                ApuBleedAirValve::new(),
                ShutdownReason::Manual,
                ThermodynamicTemperature::new::<degree_celsius>(0.),
            ))),
            egt_warning_temp: ThermodynamicTemperature::new::<degree_celsius>(
                AuxiliaryPowerUnit::WARNING_MAX_TEMPERATURE,
            ),
        }
    }

    pub fn update(
        &mut self,
        context: &UpdateContext,
        overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) {
        if let Some(state) = self.state.take() {
            self.state = Some(state.update(context, overhead, pneumatic_overhead));
        }

        self.egt_warning_temp = self.calculate_egt_warning_temp();
    }

    pub fn get_n(&self) -> Ratio {
        self.state.as_ref().unwrap().get_n()
    }

    pub fn is_available(&self) -> bool {
        self.state.as_ref().unwrap().is_available()
    }

    /// When true, the "FLAP OPEN" message on the ECAM APU page should be displayed.
    fn is_air_intake_flap_fully_open(&self) -> bool {
        self.state.as_ref().unwrap().is_air_intake_flap_fully_open()
    }

    fn get_egt(&self) -> ThermodynamicTemperature {
        self.state.as_ref().unwrap().get_egt()
    }

    fn get_egt_warning_temperature(&self) -> ThermodynamicTemperature {
        self.egt_warning_temp
    }

    fn get_egt_maximum_temperature(&self) -> ThermodynamicTemperature {
        const MAX_ABOVE_WARNING: f64 = 33.;
        ThermodynamicTemperature::new::<degree_celsius>(
            self.egt_warning_temp.get::<degree_celsius>() + MAX_ABOVE_WARNING,
        )
    }

    fn calculate_egt_warning_temp(&self) -> ThermodynamicTemperature {
        let x = match self.get_n().get::<percent>() {
            n if n < 11. => AuxiliaryPowerUnit::WARNING_MAX_TEMPERATURE,
            n if n <= 15. => (-50. * n) + 1750.,
            n if n <= 65. => (-3. * n) + 1045.,
            n => (-30. / 7. * n) + 1128.6,
        };

        ThermodynamicTemperature::new::<degree_celsius>(x)
    }
}

trait ApuState {
    fn update(
        self: Box<Self>,
        context: &UpdateContext,
        overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) -> Box<dyn ApuState>;

    fn get_n(&self) -> Ratio;

    fn is_available(&self) -> bool;

    /// When true, the "FLAP OPEN" message on the ECAM APU page should be displayed.
    fn is_air_intake_flap_fully_open(&self) -> bool;

    fn get_egt(&self) -> ThermodynamicTemperature;
}

struct Shutdown {
    air_intake_flap: AirIntakeFlap,
    bleed_air_valve: ApuBleedAirValve,
    reason: ShutdownReason,
    egt: ThermodynamicTemperature,
}
impl Shutdown {
    fn new(
        air_intake_flap: AirIntakeFlap,
        bleed_air_valve: ApuBleedAirValve,
        reason: ShutdownReason,
        egt: ThermodynamicTemperature,
    ) -> Shutdown {
        Shutdown {
            air_intake_flap,
            bleed_air_valve,
            reason,
            egt,
        }
    }
}
impl ApuState for Shutdown {
    fn update(
        mut self: Box<Self>,
        context: &UpdateContext,
        apu_overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) -> Box<dyn ApuState> {
        if apu_overhead.master_is_on() {
            self.air_intake_flap.open();
        } else {
            self.air_intake_flap.close();
        }
        self.air_intake_flap.update(context);

        self.egt = calculate_towards_ambient_egt(self.egt, context);

        self.bleed_air_valve
            .update(context, self.get_n(), apu_overhead, pneumatic_overhead);

        if self.air_intake_flap.is_fully_open()
            && apu_overhead.master_is_on()
            && apu_overhead.start_is_on()
        {
            Box::new(Starting::new(self.air_intake_flap, self.bleed_air_valve))
        } else {
            self
        }
    }

    fn get_n(&self) -> Ratio {
        Ratio::new::<percent>(0.)
    }

    fn is_available(&self) -> bool {
        false
    }

    fn is_air_intake_flap_fully_open(&self) -> bool {
        self.air_intake_flap.is_fully_open()
    }

    fn get_egt(&self) -> ThermodynamicTemperature {
        self.egt
    }
}

struct Starting {
    air_intake_flap: AirIntakeFlap,
    bleed_air_valve: ApuBleedAirValve,
    since: Duration,
    n: Ratio,
    egt: ThermodynamicTemperature,
}
impl Starting {
    fn new(air_intake_flap: AirIntakeFlap, bleed_air_valve: ApuBleedAirValve) -> Starting {
        Starting {
            air_intake_flap,
            bleed_air_valve,
            since: Duration::from_secs(0),
            n: Ratio::new::<percent>(0.),
            egt: ThermodynamicTemperature::new::<degree_celsius>(0.),
        }
    }

    fn calculate_egt(&self, context: &UpdateContext) -> ThermodynamicTemperature {
        const APU_N_TEMP_CONST: f64 = -96.565;
        const APU_N_TEMP_X: f64 = 28.571;
        const APU_N_TEMP_X2: f64 = 0.0884;
        const APU_N_TEMP_X3: f64 = -0.0081;
        const APU_N_TEMP_X4: f64 = 0.00005;

        let n = self.n.get::<percent>();

        let temperature = (APU_N_TEMP_X4 * n.powi(4))
            + (APU_N_TEMP_X3 * n.powi(3))
            + (APU_N_TEMP_X2 * n.powi(2))
            + (APU_N_TEMP_X * n)
            + APU_N_TEMP_CONST;

        ThermodynamicTemperature::new::<degree_celsius>(
            temperature.max(context.ambient_temperature.get::<degree_celsius>()),
        )
    }

    fn calculate_n(&self) -> Ratio {
        const APU_N_X: f64 = 2.375010484;
        const APU_N_X2: f64 = 0.034236847;
        const APU_N_X3: f64 = -0.007404136;
        const APU_N_X4: f64 = 0.000254;
        const APU_N_X5: f64 = -0.000002438;
        const APU_N_CONST: f64 = 0.;

        // Protect against the formula returning decreasing results when a lot of time is skipped.
        const TIME_LIMIT: f64 = 50.;
        const START_IGNITION_AFTER_SECONDS: f64 = 1.5;
        let ignition_turned_on_secs =
            (self.since.as_secs_f64() - START_IGNITION_AFTER_SECONDS).min(TIME_LIMIT);

        if ignition_turned_on_secs > 0. {
            Ratio::new::<percent>(
                ((APU_N_X5 * ignition_turned_on_secs.powi(5))
                    + (APU_N_X4 * ignition_turned_on_secs.powi(4))
                    + (APU_N_X3 * ignition_turned_on_secs.powi(3))
                    + (APU_N_X2 * ignition_turned_on_secs.powi(2))
                    + (APU_N_X * ignition_turned_on_secs)
                    + APU_N_CONST)
                    .min(100.),
            )
        } else {
            Ratio::new::<percent>(0.)
        }
    }
}
impl ApuState for Starting {
    fn update(
        mut self: Box<Self>,
        context: &UpdateContext,
        apu_overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) -> Box<dyn ApuState> {
        self.since = self.since + context.delta;
        self.n = self.calculate_n();
        self.egt = self.calculate_egt(context);

        self.air_intake_flap.update(context);

        self.bleed_air_valve
            .update(context, self.get_n(), apu_overhead, pneumatic_overhead);

        if self.n.get::<percent>() == 100. {
            Box::new(Running::new(
                self.air_intake_flap,
                self.bleed_air_valve,
                self.egt,
            ))
        } else {
            self
        }
    }

    fn get_n(&self) -> Ratio {
        self.n
    }

    fn is_available(&self) -> bool {
        false
    }

    fn is_air_intake_flap_fully_open(&self) -> bool {
        self.air_intake_flap.is_fully_open()
    }

    fn get_egt(&self) -> ThermodynamicTemperature {
        self.egt
    }
}

struct Running {
    air_intake_flap: AirIntakeFlap,
    bleed_air_valve: ApuBleedAirValve,
    egt: ThermodynamicTemperature,
}
impl Running {
    fn new(
        air_intake_flap: AirIntakeFlap,
        bleed_air_valve: ApuBleedAirValve,
        egt: ThermodynamicTemperature,
    ) -> Running {
        Running {
            air_intake_flap,
            bleed_air_valve,
            egt,
        }
    }

    fn calculate_slow_cooldown_to_running_temperature(
        &self,
        context: &UpdateContext,
    ) -> ThermodynamicTemperature {
        let mut rng = rand::thread_rng();
        let random_target_temperature: f64 = 500. - rng.gen_range(0..13) as f64;

        if self.egt.get::<degree_celsius>() > random_target_temperature {
            self.egt
                - TemperatureInterval::new::<uom::si::temperature_interval::degree_celsius>(
                    0.4 * context.delta.as_secs_f64(),
                )
        } else {
            self.egt
        }
    }

    fn is_past_bleed_air_cooldown_period(&self) -> bool {
        const BLEED_AIR_COOLDOWN_DURATION_SECS: u64 = 120;
        !self
            .bleed_air_valve
            .was_open_in_last(Duration::from_secs(BLEED_AIR_COOLDOWN_DURATION_SECS))
    }
}
impl ApuState for Running {
    fn update(
        mut self: Box<Self>,
        context: &UpdateContext,
        apu_overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) -> Box<dyn ApuState> {
        self.egt = self.calculate_slow_cooldown_to_running_temperature(context);

        self.air_intake_flap.update(context);

        self.bleed_air_valve
            .update(context, self.get_n(), apu_overhead, pneumatic_overhead);

        if apu_overhead.master_is_off() && self.is_past_bleed_air_cooldown_period() {
            Box::new(Stopping::new(
                self.air_intake_flap,
                self.bleed_air_valve,
                self.egt,
                ShutdownReason::Manual,
            ))
        } else {
            self
        }
    }

    fn get_n(&self) -> Ratio {
        Ratio::new::<percent>(100.)
    }

    fn is_available(&self) -> bool {
        true
    }

    fn is_air_intake_flap_fully_open(&self) -> bool {
        self.air_intake_flap.is_fully_open()
    }

    fn get_egt(&self) -> ThermodynamicTemperature {
        self.egt
    }
}

struct Stopping {
    air_intake_flap: AirIntakeFlap,
    bleed_air_valve: ApuBleedAirValve,
    reason: ShutdownReason,
    since: Duration,
    n: Ratio,
    egt: ThermodynamicTemperature,
}
impl Stopping {
    fn new(
        air_intake_flap: AirIntakeFlap,
        bleed_air_valve: ApuBleedAirValve,
        egt: ThermodynamicTemperature,
        reason: ShutdownReason,
    ) -> Stopping {
        Stopping {
            air_intake_flap,
            bleed_air_valve,
            since: Duration::from_secs(0),
            reason,
            n: Ratio::new::<percent>(100.),
            egt,
        }
    }

    fn calculate_n(&self, context: &UpdateContext) -> Ratio {
        const SPOOL_DOWN_COEFFICIENT: f64 = 2.;
        let mut n = self.n.get::<percent>();
        n = (n - (context.delta.as_secs_f64() * SPOOL_DOWN_COEFFICIENT)).max(0.);

        Ratio::new::<percent>(n)
    }
}
impl ApuState for Stopping {
    fn update(
        mut self: Box<Self>,
        context: &UpdateContext,
        apu_overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) -> Box<dyn ApuState> {
        self.since = self.since + context.delta;
        self.n = self.calculate_n(context);
        self.egt = calculate_towards_ambient_egt(self.egt, context);

        self.bleed_air_valve
            .update(context, self.get_n(), apu_overhead, pneumatic_overhead);

        if self.n.get::<percent>() <= 7. {
            self.air_intake_flap.close();
        }

        self.air_intake_flap.update(context);

        if self.n.get::<percent>() == 0. {
            Box::new(Shutdown::new(
                self.air_intake_flap,
                self.bleed_air_valve,
                self.reason,
                self.egt,
            ))
        } else {
            self
        }
    }

    fn get_n(&self) -> Ratio {
        self.n
    }

    fn is_available(&self) -> bool {
        false
    }

    fn is_air_intake_flap_fully_open(&self) -> bool {
        self.air_intake_flap.is_fully_open()
    }

    fn get_egt(&self) -> ThermodynamicTemperature {
        self.egt
    }
}

fn calculate_towards_ambient_egt(
    current_egt: ThermodynamicTemperature,
    context: &UpdateContext,
) -> ThermodynamicTemperature {
    const APU_AMBIENT_COEFFICIENT: f64 = 2.;

    if current_egt == context.ambient_temperature {
        current_egt
    } else if current_egt > context.ambient_temperature {
        ThermodynamicTemperature::new::<degree_celsius>(
            (current_egt.get::<degree_celsius>()
                - (APU_AMBIENT_COEFFICIENT * context.delta.as_secs_f64()))
            .max(context.ambient_temperature.get::<degree_celsius>()),
        )
    } else {
        ThermodynamicTemperature::new::<degree_celsius>(
            (current_egt.get::<degree_celsius>()
                + (APU_AMBIENT_COEFFICIENT * context.delta.as_secs_f64()))
            .min(context.ambient_temperature.get::<degree_celsius>()),
        )
    }
}

struct ApuBleedAirValve {
    valve: BleedAirValve,
    last_open_time_ago: Duration,
}
impl ApuBleedAirValve {
    fn new() -> Self {
        ApuBleedAirValve {
            valve: BleedAirValve::new(),
            last_open_time_ago: Duration::from_secs(1000),
        }
    }

    fn update(
        &mut self,
        context: &UpdateContext,
        n: Ratio,
        apu_overhead: &AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: &PneumaticOverheadPanel,
    ) {
        // Note: it might be that later we have situations in which master is on,
        // but an emergency shutdown happens and this doesn't turn off.
        // In this case, we need to modify the code below to no longer look at apu overhead state, but APU state itself.
        self.valve.open_when(
            apu_overhead.master_is_on()
                && n.get::<percent>() > 95.
                && pneumatic_overhead.apu_bleed_is_on(),
        );

        if self.valve.is_open() {
            self.last_open_time_ago = Duration::from_secs(0);
        } else {
            self.last_open_time_ago += context.delta;
        }
    }

    fn open_when(&mut self, condition: bool) {
        self.valve.open_when(condition);
    }

    fn was_open_in_last(&self, duration: Duration) -> bool {
        self.last_open_time_ago <= duration
    }
}

pub struct AuxiliaryPowerUnitOverheadPanel {
    pub master: OnOffPushButton,
    pub start: OnOffPushButton,
}
impl AuxiliaryPowerUnitOverheadPanel {
    pub fn new() -> AuxiliaryPowerUnitOverheadPanel {
        AuxiliaryPowerUnitOverheadPanel {
            master: OnOffPushButton::new_off(),
            start: OnOffPushButton::new_off(),
        }
    }

    pub fn update_after_apu(&mut self, apu: &AuxiliaryPowerUnit) {
        self.start.set_available(apu.is_available());
        if self.start_is_on() && apu.is_available() {
            self.start.turn_off();
        }
    }

    fn master_is_on(&self) -> bool {
        self.master.is_on()
    }

    fn master_is_off(&self) -> bool {
        self.master.is_off()
    }

    fn start_is_on(&self) -> bool {
        self.start.is_on()
    }

    fn start_shows_available(&self) -> bool {
        self.start.shows_available()
    }
}

impl Visitable for AuxiliaryPowerUnit {
    fn accept(&mut self, visitor: &mut Box<dyn crate::visitor::MutableVisitor>) {
        visitor.visit_auxiliary_power_unit(self);
    }
}

#[derive(Debug, PartialEq)]
enum AirIntakeFlapTarget {
    Open,
    Closed,
}

#[derive(Debug)]
struct AirIntakeFlap {
    state: Ratio,
    target: AirIntakeFlapTarget,
    delay: i32,
}
impl AirIntakeFlap {
    fn new() -> AirIntakeFlap {
        let mut rng = rand::thread_rng();
        let delay = 3 + rng.gen_range(0..13);

        AirIntakeFlap {
            state: Ratio::new::<percent>(0.),
            target: AirIntakeFlapTarget::Closed,
            delay,
        }
    }

    fn update(&mut self, context: &UpdateContext) {
        if self.target == AirIntakeFlapTarget::Open && self.state < Ratio::new::<percent>(100.) {
            self.state += Ratio::new::<percent>(
                self.get_flap_change_for_delta(context)
                    .min(100. - self.state.get::<percent>()),
            );
        } else if self.target == AirIntakeFlapTarget::Closed
            && self.state > Ratio::new::<percent>(0.)
        {
            self.state -= Ratio::new::<percent>(
                self.get_flap_change_for_delta(context)
                    .max(self.state.get::<percent>()),
            );
        }
    }

    fn get_flap_change_for_delta(&self, context: &UpdateContext) -> f64 {
        100. * (context.delta.as_secs_f64() / self.delay as f64)
    }

    fn open(&mut self) {
        self.target = AirIntakeFlapTarget::Open;
    }

    fn close(&mut self) {
        self.target = AirIntakeFlapTarget::Closed;
    }

    fn is_fully_open(&self) -> bool {
        self.state == Ratio::new::<percent>(100.)
    }
}

#[cfg(test)]
pub mod tests {
    use std::time::Duration;

    use uom::si::thermodynamic_temperature::degree_celsius;

    use crate::shared::test_helpers::context_with;

    use super::*;

    pub fn running_apu() -> AuxiliaryPowerUnit {
        tester_with().running_apu().get_apu()
    }

    pub fn stopped_apu() -> AuxiliaryPowerUnit {
        tester().get_apu()
    }

    fn tester_with() -> AuxiliaryPowerUnitTester {
        AuxiliaryPowerUnitTester::new()
    }

    fn tester() -> AuxiliaryPowerUnitTester {
        AuxiliaryPowerUnitTester::new()
    }

    struct AuxiliaryPowerUnitTester {
        apu: AuxiliaryPowerUnit,
        apu_overhead: AuxiliaryPowerUnitOverheadPanel,
        pneumatic_overhead: PneumaticOverheadPanel,
        ambient_temperature: ThermodynamicTemperature,
    }
    impl AuxiliaryPowerUnitTester {
        fn new() -> Self {
            AuxiliaryPowerUnitTester {
                apu: AuxiliaryPowerUnit::new(),
                apu_overhead: AuxiliaryPowerUnitOverheadPanel::new(),
                pneumatic_overhead: PneumaticOverheadPanel::new(),
                ambient_temperature: ThermodynamicTemperature::new::<degree_celsius>(0.),
            }
        }

        fn master_on(mut self) -> Self {
            self.apu_overhead.master.turn_on();
            self
        }

        fn master_off(mut self) -> Self {
            self.apu_overhead.master.turn_off();
            self
        }

        fn start_on(mut self) -> Self {
            self.apu_overhead.start.turn_on();
            self
        }

        fn bleed_air_off(mut self) -> Self {
            self.pneumatic_overhead.turn_apu_bleed_off();
            self
        }

        fn starting_apu(self) -> Self {
            self.master_on()
                .run(Duration::from_secs(1_000))
                .then_continue_with()
                .start_on()
                .run(Duration::from_secs(0))
        }

        fn running_apu(mut self) -> Self {
            self = self.starting_apu();
            loop {
                self = self.run(Duration::from_secs(1));
                if self.apu.is_available() {
                    break;
                }
            }

            self
        }

        fn running_apu_with_bleed_air(mut self) -> Self {
            self.pneumatic_overhead.turn_apu_bleed_on();
            self.running_apu()
        }

        fn running_apu_without_bleed_air(mut self) -> Self {
            self.pneumatic_overhead.turn_apu_bleed_off();
            self.running_apu()
        }

        fn ambient_temperature(mut self, ambient: ThermodynamicTemperature) -> Self {
            self.ambient_temperature = ambient;
            self
        }

        fn and(self) -> Self {
            self
        }

        fn then_continue_with(self) -> Self {
            self
        }

        fn run(mut self, delta: Duration) -> Self {
            self.apu.update(
                &context_with()
                    .delta(delta)
                    .and()
                    .ambient_temperature(self.ambient_temperature)
                    .build(),
                &self.apu_overhead,
                &self.pneumatic_overhead,
            );

            self.apu_overhead.update_after_apu(&self.apu);

            self
        }

        fn is_air_intake_flap_fully_open(&self) -> bool {
            self.apu.is_air_intake_flap_fully_open()
        }

        fn get_n(&self) -> Ratio {
            self.apu.get_n()
        }

        fn get_egt(&self) -> ThermodynamicTemperature {
            self.apu.get_egt()
        }

        fn get_egt_maximum_temperature(&self) -> ThermodynamicTemperature {
            self.apu.get_egt_maximum_temperature()
        }

        fn get_egt_warning_temperature(&self) -> ThermodynamicTemperature {
            self.apu.get_egt_warning_temperature()
        }

        fn apu_is_available(&self) -> bool {
            self.apu.is_available()
        }

        fn start_is_on(&self) -> bool {
            self.apu_overhead.start_is_on()
        }

        fn start_shows_available(&self) -> bool {
            self.apu_overhead.start_shows_available()
        }

        fn get_apu(self) -> AuxiliaryPowerUnit {
            self.apu
        }
    }

    #[cfg(test)]
    mod apu_tests {
        use ntest::{assert_about_eq, timeout};

        use super::*;

        #[test]
        fn when_apu_master_sw_turned_on_air_intake_flap_opens() {
            let tester = tester_with().master_on().run(Duration::from_secs(20));

            assert_eq!(tester.is_air_intake_flap_fully_open(), true)
        }

        #[test]
        fn when_start_sw_on_apu_starts_within_expected_time() {
            const APPROXIMATE_STARTUP_TIME: u64 = 49;

            let tester = tester_with()
                .starting_apu()
                .run(Duration::from_secs(APPROXIMATE_STARTUP_TIME));

            assert_eq!(tester.get_n().get::<percent>(), 100.);
        }

        #[test]
        fn one_and_a_half_seconds_after_starting_sequence_commences_ignition_starts() {
            let tester = tester_with()
                .starting_apu()
                .run(Duration::from_millis(1500));

            assert_eq!(
                tester.get_n().get::<percent>(),
                0.,
                "Ignition started too early."
            );

            let tester = tester.then_continue_with().run(Duration::from_millis(1));

            assert!(
                tester.get_n().get::<percent>() > 0.,
                "Ignition started too late."
            );
        }

        #[test]
        fn when_apu_not_started_egt_is_ambient() {
            const AMBIENT_TEMPERATURE: f64 = 0.;

            let tester = tester_with()
                .ambient_temperature(ThermodynamicTemperature::new::<degree_celsius>(
                    AMBIENT_TEMPERATURE,
                ))
                .run(Duration::from_secs(1_000));

            assert_eq!(
                tester.get_egt().get::<degree_celsius>(),
                AMBIENT_TEMPERATURE
            );
        }

        #[test]
        fn when_ambient_temperature_high_startup_egt_never_below_ambient() {
            const AMBIENT_TEMPERATURE: f64 = 50.;

            let tester = tester_with()
                .starting_apu()
                .and()
                .ambient_temperature(ThermodynamicTemperature::new::<degree_celsius>(
                    AMBIENT_TEMPERATURE,
                ))
                .run(Duration::from_secs(1));

            assert_eq!(
                tester.get_egt().get::<degree_celsius>(),
                AMBIENT_TEMPERATURE
            );
        }

        #[test]
        fn when_apu_starting_egt_reaches_above_800_degree_celsius() {
            let mut tester = tester_with().starting_apu();
            let mut max_egt: f64 = 0.;

            loop {
                tester = tester.run(Duration::from_secs(1));

                let egt = tester.get_egt().get::<degree_celsius>();
                if egt < max_egt {
                    break;
                }

                max_egt = egt;
            }

            assert!(max_egt > 800.);
        }

        #[test]
        fn egt_max_always_33_above_egt_warn() {
            let mut tester = tester_with().starting_apu();

            for _ in 1..=100 {
                tester = tester.run(Duration::from_secs(1));

                assert_about_eq!(
                    tester.get_egt_maximum_temperature().get::<degree_celsius>(),
                    tester.get_egt_warning_temperature().get::<degree_celsius>() + 33.
                );
            }
        }

        #[test]
        fn start_sw_on_light_turns_off_when_apu_available() {
            let mut tester = tester_with().starting_apu();

            loop {
                tester = tester.run(Duration::from_secs(1));

                if tester.apu_is_available() {
                    break;
                }
            }

            assert!(!tester.start_is_on());
            assert!(tester.start_shows_available());
        }

        #[test]
        fn when_apu_bleed_valve_open_on_shutdown_cooldown_period_commences_and_apu_remains_available(
        ) {
            // The cool down period is between 60 to 120. It is configurable by aircraft mechanics and
            // we'll make it a configurable option in the sim. For now, 120s.
            let tester = tester_with()
                .running_apu()
                .and()
                .master_off()
                .run(Duration::from_secs(120));

            assert!(tester.apu_is_available());

            let tester = tester.run(Duration::from_millis(1));

            assert!(!tester.apu_is_available());
        }

        #[test]
        fn when_apu_bleed_valve_was_open_recently_on_shutdown_cooldown_period_commences_and_apu_remains_available(
        ) {
            // The cool down period requires that the bleed valve is shut for a duration (default 120s).
            // If the bleed valve was shut earlier than the MASTER SW going to OFF, that time period counts towards the cool down period.

            let tester = tester_with()
                .running_apu_with_bleed_air()
                .and()
                .bleed_air_off()
                .run(Duration::from_secs(80));

            assert!(tester.apu_is_available());

            let tester = tester.master_off().run(Duration::from_secs(40));

            assert!(tester.apu_is_available());

            let tester = tester.run(Duration::from_millis(1));

            assert!(!tester.apu_is_available());
        }

        #[test]
        fn when_apu_bleed_valve_closed_on_shutdown_cooldown_period_is_skipped_and_apu_stops() {
            let tester = tester_with().running_apu_without_bleed_air();

            assert!(tester.apu_is_available());

            let tester = tester.master_off().run(Duration::from_millis(1));

            assert!(!tester.apu_is_available());
        }

        #[test]
        #[ignore]
        fn when_egt_is_greater_than_egt_max_automatic_shutdown_begins() {
            // Note should also test 2 seconds after reaching 95 the light turns off?
        }

        #[test]
        fn when_apu_shutting_down_at_7_percent_n_air_inlet_flap_closes() {
            let mut tester = tester_with().running_apu().and().master_off();

            loop {
                tester = tester.run(Duration::from_secs(1));

                if tester.get_n().get::<percent>() <= 7. {
                    break;
                }
            }

            assert!(!tester.is_air_intake_flap_fully_open());
        }

        #[test]
        #[timeout(500)]
        fn apu_cools_down_to_ambient_temperature_after_running() {
            let ambient = ThermodynamicTemperature::new::<degree_celsius>(10.);
            let mut tester = tester_with()
                .running_apu()
                .ambient_temperature(ambient)
                .and()
                .master_off();

            while tester.get_egt() != ambient {
                tester = tester.run(Duration::from_secs(1));
            }
        }

        #[test]
        fn shutdown_apu_warms_up_as_ambient_temperature_increases() {
            let starting_temperature = ThermodynamicTemperature::new::<degree_celsius>(0.);
            let tester = tester_with().ambient_temperature(starting_temperature);

            let tester = tester.run(Duration::from_secs(1_000));

            let target_temperature = ThermodynamicTemperature::new::<degree_celsius>(20.);

            let tester = tester
                .then_continue_with()
                .ambient_temperature(target_temperature)
                .run(Duration::from_secs(1_000));

            assert_eq!(tester.get_egt(), target_temperature);
        }
    }

    #[cfg(test)]
    mod air_intake_flap_tests {
        use crate::shared::test_helpers::context_with;

        use super::*;

        #[test]
        fn starts_opening_when_target_is_open() {
            let mut flap = AirIntakeFlap::new();
            flap.open();

            flap.update(&context_with().delta(Duration::from_secs(5)).build());

            assert!(flap.state.get::<percent>() > 0.);
        }

        #[test]
        fn closes_when_target_is_closed() {
            let mut flap = AirIntakeFlap::new();
            flap.open();
            flap.update(&context_with().delta(Duration::from_secs(5)).build());
            let open_percentage = flap.state.get::<percent>();

            flap.close();
            flap.update(&context_with().delta(Duration::from_secs(2)).build());

            assert!(flap.state.get::<percent>() < open_percentage);
        }

        #[test]
        fn never_closes_beyond_0_percent() {
            let mut flap = AirIntakeFlap::new();
            flap.close();
            flap.update(&context_with().delta(Duration::from_secs(1_000)).build());

            assert_eq!(flap.state.get::<percent>(), 0.);
        }

        #[test]
        fn never_opens_beyond_100_percent() {
            let mut flap = AirIntakeFlap::new();
            flap.open();
            flap.update(&context_with().delta(Duration::from_secs(1_000)).build());

            assert_eq!(flap.state.get::<percent>(), 100.);
        }

        #[test]
        fn is_fully_open_returns_false_when_closed() {
            let flap = AirIntakeFlap::new();

            assert_eq!(flap.is_fully_open(), false)
        }

        #[test]
        fn is_fully_open_returns_true_when_open() {
            let mut flap = AirIntakeFlap::new();
            flap.open();
            flap.update(&context_with().delta(Duration::from_secs(1_000)).build());

            assert_eq!(flap.is_fully_open(), true)
        }
    }
}
